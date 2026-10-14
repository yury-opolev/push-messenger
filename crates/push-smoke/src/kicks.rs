//! Kicks, as the gateway will receive them (Plan 8 closes the sockets): the smoke test
//! listens on the devices' Redis channels while auth-svc revokes a device and resets a
//! password, and the worker relays both to message-svc.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use futures_util::StreamExt;
use prost::Message;
use push_proto::internal::v1::{Kick, KickReason};
use push_proto::kick_channel;
use redis::aio::PubSub;
use uuid::Uuid;

use crate::WORKER_TIMEOUT;

/// A subscription to the kick channels of some devices.
pub(crate) struct Kicks {
    pubsub: PubSub,
}

impl Kicks {
    /// Subscribes to `kick:{device_id}` of each device. Errors never quote `url`, which
    /// holds the Redis password.
    pub async fn subscribe(url: &str, devices: &[Uuid]) -> anyhow::Result<Self> {
        let unreachable = |error: redis::RedisError| anyhow!("Redis: {:?}", error.kind());
        let client = redis::Client::open(url).map_err(unreachable)?;
        let mut pubsub = client.get_async_pubsub().await.map_err(unreachable)?;
        for device in devices {
            pubsub
                .subscribe(kick_channel(*device))
                .await
                .map_err(unreachable)?;
        }
        Ok(Self { pubsub })
    }

    /// Waits for `device` to be kicked for `reason`; how long that took.
    pub async fn kicked(&mut self, device: Uuid, reason: KickReason) -> anyhow::Result<Duration> {
        let started = Instant::now();
        let channel = kick_channel(device);
        let mut messages = self.pubsub.on_message();
        let Ok(message) = tokio::time::timeout(WORKER_TIMEOUT, messages.next()).await else {
            bail!("{device} was not kicked within {WORKER_TIMEOUT:?}");
        };
        let message = message.ok_or_else(|| anyhow!("the Redis subscription ended"))?;
        if message.get_channel_name() != channel {
            bail!(
                "waiting for {device} to be kicked, but {} was kicked instead",
                message.get_channel_name()
            );
        }
        let kick = Kick::decode(message.get_payload_bytes()).context("decode a kick")?;
        if kick.reason() != reason {
            bail!(
                "{device} was kicked for {:?}, not {reason:?}",
                kick.reason()
            );
        }
        Ok(started.elapsed())
    }
}
