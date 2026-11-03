//! Minting credentials through a TURN provider's API.
//!
//! - Cloudflare Realtime TURN
//!   (<https://developers.cloudflare.com/realtime/turn/generate-credentials/>):
//!   `POST /v1/turn/keys/{key id}/credentials/generate-ice-servers` with the API token as
//!   a bearer token and `{"ttl": seconds}`; `201` with `{"iceServers": [{urls, username?,
//!   credential?}]}`. URLs on port 53 are dropped: browsers block that port.
//! - Twilio Network Traversal Service
//!   (<https://www.twilio.com/docs/stun-turn/api>): `POST
//!   /2010-04-01/Accounts/{sid}/Tokens.json` with Basic auth (account SID, auth token)
//!   and the form `Ttl=seconds`; `201` with `{"ice_servers": [{urls, username?,
//!   credential?}], "ttl": "seconds"}`, one URL per entry.
//!
//! Only `stun:`, `stuns:`, `turn:` and `turns:` URLs are kept, a TURN URL only with its
//! username and credential; an answer without a usable relay is an error. Bodies and
//! credentials are never logged; errors carry the status only.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;

use super::{IceServer, MAX_URL_BYTES, MAX_URLS, Source, TurnSettings};

pub const CLOUDFLARE_BASE_URL: &str = "https://rtc.live.cloudflare.com";
pub const TWILIO_BASE_URL: &str = "https://api.twilio.com";

/// The longest a provider call may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The largest answer read.
const MAX_ANSWER_BYTES: usize = 64 * 1024;
/// The longest username or credential kept.
const MAX_SECRET_BYTES: usize = 512;

/// Why a provider gave no credentials.
#[derive(Debug, thiserror::Error)]
pub(super) enum ProviderError {
    #[error("the provider answered HTTP {0}")]
    Status(u16),
    #[error("the provider could not be reached: {0}")]
    Transport(String),
    #[error("the provider's answer has no usable TURN server")]
    Unusable,
}

/// A provider's HTTP client.
#[derive(Debug)]
pub(super) struct Provider {
    client: reqwest::Client,
    base_url: String,
}

impl Provider {
    pub(super) fn new(base_url: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("the TURN provider client: {error}"))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// New credentials for `settings` (a provider source): the servers and their expiry.
    pub(super) async fn mint(
        &self,
        settings: &TurnSettings,
        now: DateTime<Utc>,
    ) -> Result<(Vec<IceServer>, DateTime<Utc>), ProviderError> {
        let ttl = settings.ttl_seconds;
        let request = match &settings.source {
            Source::Cloudflare { key_id, token } => self
                .client
                .post(format!(
                    "{}/v1/turn/keys/{key_id}/credentials/generate-ice-servers",
                    self.base_url
                ))
                .bearer_auth(token.expose())
                .json(&serde_json::json!({ "ttl": ttl })),
            Source::Twilio { account_sid, token } => self
                .client
                .post(format!(
                    "{}/2010-04-01/Accounts/{account_sid}/Tokens.json",
                    self.base_url
                ))
                .basic_auth(account_sid, Some(token.expose()))
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(format!("Ttl={ttl}")),
            Source::Rest { .. } | Source::StunOnly => return Err(ProviderError::Unusable),
        };
        let response = request
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::Status(status.as_u16()));
        }
        let body = read_limited(response).await?;
        let answer: Answer = serde_json::from_slice(&body).map_err(|_| ProviderError::Unusable)?;
        let lifetime = answer
            .ttl()
            .map_or(ttl, |provider_ttl| provider_ttl.min(ttl));
        let servers = usable(answer.servers())?;
        Ok((servers, now + TimeDelta::seconds(i64::from(lifetime))))
    }
}

async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ProviderError::Transport(error.without_url().to_string()))?
    {
        if body.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(ProviderError::Unusable);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(one) => vec![one],
            Self::Many(many) => many,
        }
    }
}

#[derive(Deserialize)]
struct RawServer {
    #[serde(default)]
    urls: Option<OneOrMany<String>>,
    /// The older name of `urls` (Twilio still sends both).
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    credential: Option<String>,
}

/// Cloudflare's and Twilio's answers in one shape.
#[derive(Deserialize)]
struct Answer {
    #[serde(default, rename = "iceServers")]
    cloudflare: Option<OneOrMany<RawServer>>,
    #[serde(default, rename = "ice_servers")]
    twilio: Option<Vec<RawServer>>,
    /// Twilio: the lifetime in seconds, as a string.
    #[serde(default)]
    ttl: Option<serde_json::Value>,
}

impl Answer {
    fn ttl(&self) -> Option<u32> {
        match self.ttl.as_ref()? {
            serde_json::Value::String(text) => text.trim().parse().ok(),
            serde_json::Value::Number(number) => {
                number.as_u64().and_then(|n| u32::try_from(n).ok())
            }
            _ => None,
        }
    }

    fn servers(self) -> Vec<RawServer> {
        match (self.cloudflare, self.twilio) {
            (Some(servers), _) => servers.into_vec(),
            (None, Some(servers)) => servers,
            (None, None) => Vec::new(),
        }
    }
}

/// The usable servers of an answer; entries with the same credentials are merged.
fn usable(raw: Vec<RawServer>) -> Result<Vec<IceServer>, ProviderError> {
    let mut servers: Vec<IceServer> = Vec::new();
    for server in raw {
        let mut urls = server.urls.map(OneOrMany::into_vec).unwrap_or_default();
        if urls.is_empty() {
            urls.extend(server.url);
        }
        let credentials = match (server.username, server.credential) {
            (Some(username), Some(credential)) if fits(&username) && fits(&credential) => {
                Some((username, credential))
            }
            _ => None,
        };
        let (stun, turn): (Vec<String>, Vec<String>) = urls
            .into_iter()
            .filter(|url| url.len() <= MAX_URL_BYTES && url.bytes().all(|b| b.is_ascii_graphic()))
            .filter(|url| !on_port_53(url))
            .filter(|url| scheme(url).is_some())
            .partition(|url| matches!(scheme(url), Some("stun:" | "stuns:")));
        push(&mut servers, stun, None);
        if let Some(credentials) = credentials {
            push(&mut servers, turn, Some(credentials));
        }
    }
    let has_relay = servers.iter().any(|server| server.credential.is_some());
    if !has_relay || servers.len() > MAX_URLS {
        return Err(ProviderError::Unusable);
    }
    Ok(servers)
}

fn push(servers: &mut Vec<IceServer>, urls: Vec<String>, credentials: Option<(String, String)>) {
    if urls.is_empty() {
        return;
    }
    let (username, credential) = match credentials {
        Some((username, credential)) => (Some(username), Some(credential)),
        None => (None, None),
    };
    if let Some(last) = servers.last_mut() {
        if last.username == username && last.credential == credential {
            last.urls.extend(urls);
            return;
        }
    }
    servers.push(IceServer {
        urls,
        username,
        credential,
    });
}

fn fits(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SECRET_BYTES
        && value.bytes().all(|b| b.is_ascii_graphic())
}

fn scheme(url: &str) -> Option<&'static str> {
    ["stun:", "stuns:", "turn:", "turns:"]
        .into_iter()
        .find(|scheme| {
            url.get(..scheme.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
        })
}

/// Whether the URL names port 53, which browsers refuse to connect to.
fn on_port_53(url: &str) -> bool {
    let authority = url.split('?').next().unwrap_or(url);
    authority.ends_with(":53")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Vec<IceServer>, ProviderError> {
        let answer: Answer = serde_json::from_str(json).unwrap();
        usable(answer.servers())
    }

    #[test]
    fn cloudflare_answers_keep_their_servers_without_port_53() {
        let servers = parse(
            r#"{"iceServers":[{"urls":["stun:stun.cloudflare.com:3478","stun:stun.cloudflare.com:53"]},
            {"urls":["turn:turn.cloudflare.com:3478?transport=udp","turn:turn.cloudflare.com:53?transport=udp",
            "turns:turn.cloudflare.com:443?transport=tcp"],"username":"u","credential":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, ["stun:stun.cloudflare.com:3478"]);
        assert_eq!(servers[0].credential, None);
        assert_eq!(
            servers[1].urls,
            [
                "turn:turn.cloudflare.com:3478?transport=udp",
                "turns:turn.cloudflare.com:443?transport=tcp"
            ]
        );
        assert_eq!(servers[1].username.as_deref(), Some("u"));
    }

    #[test]
    fn an_object_instead_of_a_list_is_accepted() {
        let servers = parse(
            r#"{"iceServers":{"urls":["stun:s:3478","turn:t:3478"],"username":"u","credential":"c"}}"#,
        )
        .unwrap();
        assert_eq!(servers[0].urls, ["stun:s:3478"]);
        assert_eq!(servers[1].urls, ["turn:t:3478"]);
    }

    #[test]
    fn twilio_entries_with_one_url_each_are_merged() {
        let servers = parse(
            r#"{"ttl":"600","ice_servers":[{"urls":"stun:global.stun.twilio.com:3478","url":"stun:global.stun.twilio.com:3478"},
            {"urls":"turn:global.turn.twilio.com:3478?transport=udp","username":"u","credential":"c"},
            {"urls":"turn:global.turn.twilio.com:443?transport=tcp","username":"u","credential":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].urls.len(), 2);
    }

    #[test]
    fn answers_without_a_usable_relay_are_refused() {
        for json in [
            r#"{"iceServers":[{"urls":["stun:s:3478"]}]}"#,
            r#"{"iceServers":[{"urls":["turn:t:3478"],"username":"u"}]}"#,
            r#"{"iceServers":[{"urls":["http://evil/"],"username":"u","credential":"c"}]}"#,
            r#"{"iceServers":[{"urls":["turn:t:53"],"username":"u","credential":"c"}]}"#,
            r#"{"iceServers":[{"urls":["turn:t:3478"],"username":"u","credential":"bad value"}]}"#,
            r#"{"ice_servers":[]}"#,
            r#"{}"#,
        ] {
            assert!(
                matches!(parse(json), Err(ProviderError::Unusable)),
                "{json}"
            );
        }
    }

    #[test]
    fn twilio_ttl_is_read_as_a_string_or_a_number() {
        for (json, ttl) in [
            (r#"{"ttl":"600"}"#, Some(600)),
            (r#"{"ttl":120}"#, Some(120)),
            (r#"{"ttl":"x"}"#, None),
            (r#"{}"#, None),
        ] {
            let answer: Answer = serde_json::from_str(json).unwrap();
            assert_eq!(answer.ttl(), ttl, "{json}");
        }
    }
}
