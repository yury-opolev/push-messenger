//! The voice adapter as `status` and the tray see it ([`AdapterStatus`]): its state, the
//! name, version and capabilities from its hello, why it backs off, and the counters.
//! Content-free (A15): no transcript, speech or audio, only states, names, counts and
//! durations.

use std::time::Duration;

use push_bridge_control::{AdapterMetrics, AdapterStatus, LatencyInfo};
use push_voice_adapter_proto::Capabilities;
use tokio::time::Instant;

use super::router::{AdapterSetup, Latency};
use super::supervisor::AdapterState;

/// Characters of a backoff reason on the control channel.
const REASON: usize = 200;

/// The capability names of `capabilities`, in a fixed order.
pub fn capability_names(capabilities: &Capabilities) -> Vec<String> {
    [
        (capabilities.stt, "stt"),
        (capabilities.tts, "tts"),
        (capabilities.vad, "vad"),
        (capabilities.barge_in, "barge_in"),
        (capabilities.partials, "partials"),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| name.to_owned())
    .collect()
}

/// `state` on the control channel, without counters; `now_ms` is now in Unix ms (a
/// backoff's end is reported in it).
pub fn adapter_status(state: &AdapterState, now_ms: i64) -> AdapterStatus {
    match state {
        AdapterState::Off => status("off"),
        AdapterState::Starting => status("starting"),
        AdapterState::Handshaking => status("handshaking"),
        AdapterState::Ready(info) => AdapterStatus {
            name: Some(info.name.clone()),
            version: Some(info.version.clone()),
            capabilities: Some(capability_names(&info.capabilities)),
            ..status("ready")
        },
        AdapterState::Backoff { until, reason } => {
            let left = until.saturating_duration_since(Instant::now());
            let left_ms = i64::try_from(left.as_millis()).unwrap_or(i64::MAX);
            AdapterStatus {
                reason: Some(reason.chars().take(REASON).collect()),
                retry_at: Some(now_ms.saturating_add(left_ms)),
                ..status("backoff")
            }
        }
    }
}

fn status(state: &str) -> AdapterStatus {
    AdapterStatus {
        state: state.to_owned(),
        ..AdapterStatus::default()
    }
}

/// The adapter of `setup` with its counters: the router's over every call, the
/// supervisor's restarts, and the current connection's drops.
pub fn report(setup: &AdapterSetup, now_ms: i64) -> AdapterStatus {
    let state = setup.supervisor.state().borrow().clone();
    let mut status = adapter_status(&state, now_ms);
    let calls = setup.metrics.snapshot();
    let link = setup
        .supervisor
        .current()
        .borrow()
        .as_ref()
        .map(|ready| ready.link().stats());
    status.metrics = Some(AdapterMetrics {
        calls: calls.calls,
        frames_to_adapter: calls.frames_to_adapter,
        frames_from_adapter: calls.frames_from_adapter,
        dropped_to_adapter: link.as_ref().map_or(0, |link| link.dropped_to_adapter),
        dropped_from_adapter: calls.frames_dropped
            + link.as_ref().map_or(0, |link| link.dropped_from_adapter),
        restarts: setup.supervisor.restarts(),
        speaks: calls.speaks,
        final_to_speak: latency(&calls.final_to_speak),
        speak_to_audio: latency(&calls.speak_to_audio),
    });
    status
}

fn latency(latency: &Latency) -> Option<LatencyInfo> {
    let ms = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    Some(LatencyInfo {
        last_ms: ms(latency.last),
        avg_ms: ms(latency.average()?),
        max_ms: ms(latency.max),
    })
}

/// Logs the counters over the bridge's run (at its end): `adapter_metrics_total`.
pub fn log_totals(setup: &AdapterSetup) {
    let calls = setup.metrics.snapshot();
    let ms = |latency: &Latency, pick: fn(&Latency) -> Option<Duration>| {
        pick(latency).map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    };
    let last = |latency: &Latency| (latency.count > 0).then_some(latency.last);
    let max = |latency: &Latency| (latency.count > 0).then_some(latency.max);
    tracing::info!(
        event = "adapter_metrics_total",
        calls = calls.calls,
        frames_to_adapter = calls.frames_to_adapter,
        frames_from_adapter = calls.frames_from_adapter,
        frames_dropped = calls.frames_dropped,
        speaks = calls.speaks,
        restarts = setup.supervisor.restarts(),
        final_to_speak_last_ms = ms(&calls.final_to_speak, last),
        final_to_speak_avg_ms = ms(&calls.final_to_speak, Latency::average),
        final_to_speak_max_ms = ms(&calls.final_to_speak, max),
        speak_to_audio_last_ms = ms(&calls.speak_to_audio, last),
        speak_to_audio_avg_ms = ms(&calls.speak_to_audio, Latency::average),
        speak_to_audio_max_ms = ms(&calls.speak_to_audio, max),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::adapter::link::AdapterInfo;

    #[tokio::test(start_paused = true)]
    async fn states_read_as_documented() {
        assert_eq!(adapter_status(&AdapterState::Off, 0).state, "off");
        assert_eq!(adapter_status(&AdapterState::Starting, 0).state, "starting");
        let ready = adapter_status(
            &AdapterState::Ready(AdapterInfo {
                protocol: "push-voice-adapter/1".into(),
                name: "whisper".into(),
                version: "1.2.3".into(),
                capabilities: Capabilities {
                    stt: true,
                    vad: true,
                    barge_in: true,
                    ..Capabilities::default()
                },
                voices: Vec::new(),
            }),
            0,
        );
        assert_eq!(
            ready,
            AdapterStatus {
                state: "ready".into(),
                name: Some("whisper".into()),
                version: Some("1.2.3".into()),
                capabilities: Some(vec!["stt".into(), "vad".into(), "barge_in".into()]),
                ..AdapterStatus::default()
            }
        );
        let backoff = adapter_status(
            &AdapterState::Backoff {
                until: Instant::now() + Duration::from_millis(4_000),
                reason: "x".repeat(300),
            },
            1_000,
        );
        assert_eq!(backoff.state, "backoff");
        assert_eq!(backoff.retry_at, Some(5_000));
        assert_eq!(backoff.reason.unwrap().len(), REASON);
    }
}
