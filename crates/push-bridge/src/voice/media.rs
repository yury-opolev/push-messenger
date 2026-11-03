//! The app's [`Phone`]: answers the owner's calls with push-sdk's media helper
//! ([`MediaCall`], feature `voice`).

use std::sync::Arc;

use push_sdk::media::{MediaCall, MediaEnded, MediaOptions, PcmFrame, PcmSink, Role};
use push_sdk::{CallEvent, CallId, Calls, Client, SdkError};
use tokio::sync::mpsc;

use super::Frame;
use super::phone::{Line, LineControl, LineEnded, Phone, Speaker};

/// Frames of the owner's audio waiting for the pipeline (5 s); newer ones are dropped
/// when it falls further behind.
const INCOMING_FRAMES: usize = 250;

/// Answers calls with push-sdk's media helper.
pub struct MediaPhone {
    calls: Calls,
    options: MediaOptions,
}

impl MediaPhone {
    pub fn new(client: &Client) -> Self {
        Self {
            calls: client.calls(),
            options: MediaOptions::default(),
        }
    }

    pub fn with_options(mut self, options: MediaOptions) -> Self {
        self.options = options;
        self
    }
}

#[async_trait::async_trait]
impl Phone for MediaPhone {
    async fn answer(&self, call_id: CallId, offer_sdp: &str) -> Result<Line, SdkError> {
        let servers = self.calls.ice_servers().await?;
        let role = Role::Callee {
            call_id,
            offer_sdp: offer_sdp.to_owned(),
        };
        let mut call = MediaCall::start(&self.calls, &servers, role, self.options.clone()).await?;
        let (sender, incoming) = mpsc::channel(INCOMING_FRAMES);
        if let Some(mut stream) = call.incoming() {
            tokio::spawn(async move {
                while let Some(frame) = stream.recv().await {
                    // Never block the helper: a pipeline that far behind loses frames.
                    if let Err(mpsc::error::TrySendError::Closed(_)) =
                        sender.try_send(*frame.samples())
                    {
                        return;
                    }
                }
            });
        }
        let speaker = Arc::new(MediaSpeaker {
            sink: call.outgoing(),
        });
        Ok(Line {
            call_id: call.call_id(),
            incoming,
            speaker,
            control: Arc::new(MediaControl {
                call: Arc::new(call),
            }),
        })
    }

    async fn decline(&self, call_id: CallId, busy: bool) -> Result<(), SdkError> {
        self.calls.decline(call_id, busy).await
    }
}

struct MediaSpeaker {
    sink: PcmSink,
}

#[async_trait::async_trait]
impl Speaker for MediaSpeaker {
    async fn send(&self, frame: Frame) -> bool {
        self.sink.send(PcmFrame(frame)).await
    }

    fn clear(&self) {
        self.sink.clear();
    }
}

/// Keeps the call alive: dropping the last [`MediaCall`] hangs up.
struct MediaControl {
    call: Arc<MediaCall>,
}

fn line_ended(ended: MediaEnded) -> LineEnded {
    LineEnded {
        reason: ended.reason,
        duration_s: ended.duration_s,
    }
}

#[async_trait::async_trait]
impl LineControl for MediaControl {
    fn handle(&self, event: &CallEvent) -> bool {
        self.call.handle(event)
    }

    async fn connected(&self) -> Result<(), LineEnded> {
        self.call.connected().await.map_err(line_ended)
    }

    async fn ended(&self) -> LineEnded {
        line_ended(self.call.on_ended().await)
    }

    async fn hangup(&self) -> LineEnded {
        line_ended(self.call.hangup().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_media_phone_is_a_phone() {
        fn phone<P: Phone>() {}
        phone::<MediaPhone>();
    }
}
