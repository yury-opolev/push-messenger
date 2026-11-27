//! An external token validator (request "PUSH seam", 2026-11-27; header protocol:
//! request "PUSH headers", 2026-11-27): when `PUSH_ENTRA_VALIDATOR_URL` is set, the
//! signature and protocol checks of `entra` and `entra-app` tokens are delegated to it over
//! HTTP, and push-auth re-checks the claims it returns. The token is sent in the
//! `Authorization` header only and never logged; nothing of an answer reaches a client.

mod claims;
mod config;
mod headers;

use reqwest::Url;
use reqwest::header::{
    ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE,
};
use serde_json::{Map, Value};

use super::{EntraError, Reason};
use crate::identity::{ENTRA_APP_PROVIDER, ENTRA_PROVIDER, RequestMeta};

pub(crate) use claims::{Presented, recheck};
use config::is_loopback;
pub use config::{Protocol, ValidatorSettings};

/// The largest validator answer read, in bytes.
pub const MAX_VALIDATOR_BODY: usize = 16 * 1024;
const SECRET_HEADER: &str = "x-push-validator-secret";
const ORIGINAL_URI_HEADER: &str = "original-uri";
const ORIGINAL_METHOD_HEADER: &str = "original-method";
const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";
/// The most of a header-protocol answer's body read (and ignored), in bytes.
const MAX_DRAINED_BODY: usize = 4 * 1024;
/// The longest `error` of a `WWW-Authenticate` challenge logged, in characters.
const MAX_CHALLENGE_ERROR_CHARS: usize = 64;

/// Which Entra provider asks: the validator may apply token-type rules of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderKind {
    User,
    App,
}

impl ProviderKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => ENTRA_PROVIDER,
            Self::App => ENTRA_APP_PROVIDER,
        }
    }
}

/// The HTTP client of an external token validator. Follows no redirect, retries nothing
/// and caches nothing.
pub(crate) struct Validator {
    http: reqwest::Client,
    url: Url,
    secret: Option<HeaderValue>,
    protocol: Protocol,
    request_headers: Vec<(HeaderName, HeaderValue)>,
    correlation_header: Option<HeaderName>,
    claim_prefix: String,
    encoded_claim_prefix: Option<String>,
}

impl Validator {
    pub(crate) fn new(settings: &ValidatorSettings) -> Result<Self, String> {
        let url = Url::parse(&settings.url)
            .map_err(|_| "the token validator URL is not a URL".to_owned())?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(settings.timeout)
            .connect_timeout(settings.timeout);
        // A pod's HTTPS_PROXY must not catch a sidecar.
        if is_loopback(&url) {
            builder = builder.no_proxy();
        }
        if settings.protocol == Protocol::Headers && settings.request_headers.is_empty() {
            tracing::warn!(
                "the token validator uses the header protocol, but no \
                 PUSH_ENTRA_VALIDATOR_REQUEST_HEADERS are set: many forward-auth validators \
                 return claims only when a request header asks for them"
            );
        }
        let http = builder
            .build()
            .map_err(|_| "cannot build the HTTP client for the token validator".to_owned())?;
        let secret = match &settings.secret {
            None => None,
            Some(secret) => {
                let mut value = HeaderValue::from_str(secret).map_err(|_| {
                    "the token validator secret is not a valid header value".to_owned()
                })?;
                value.set_sensitive(true);
                Some(value)
            }
        };
        Ok(Self {
            http,
            url,
            secret,
            protocol: settings.protocol,
            request_headers: settings.request_headers.clone(),
            correlation_header: settings.correlation_header.clone(),
            claim_prefix: settings.claim_prefix.clone(),
            encoded_claim_prefix: settings.encoded_claim_prefix.clone(),
        })
    }

    /// Whether the answer must carry `ver`: the JSON schema requires it, while the header
    /// protocol leaves a missing `ver` to the version rule.
    pub(crate) fn requires_version(&self) -> bool {
        self.protocol == Protocol::Json
    }

    /// A `POST` with the token, the secret, what is known of the sign-in request (when it
    /// is visible ASCII) and the static headers.
    fn request(&self, token: &str, meta: &RequestMeta) -> reqwest::RequestBuilder {
        let mut request = self.http.post(self.url.clone()).bearer_auth(token);
        if let Some(secret) = &self.secret {
            request = request.header(SECRET_HEADER, secret.clone());
        }
        let client_ip = meta.client_ip.map(|ip| ip.to_string());
        let known = [
            (self.correlation_header.as_ref(), meta.request_id.as_deref()),
            (
                Some(&HeaderName::from_static(ORIGINAL_URI_HEADER)),
                meta.original_uri.as_deref(),
            ),
            (
                Some(&HeaderName::from_static(ORIGINAL_METHOD_HEADER)),
                meta.method.as_deref(),
            ),
            (
                Some(&HeaderName::from_static(FORWARDED_FOR_HEADER)),
                client_ip.as_deref(),
            ),
        ];
        for (name, value) in known {
            if let (Some(name), Some(value)) = (name, value.and_then(visible)) {
                request = request.header(name.clone(), value);
            }
        }
        for (name, value) in &self.request_headers {
            request = request.header(name.clone(), value.clone());
        }
        request
    }

    /// The validated claims of `token`, as the validator returns them.
    pub(crate) async fn check(
        &self,
        token: &str,
        kind: ProviderKind,
        meta: &RequestMeta,
    ) -> Result<Map<String, Value>, EntraError> {
        match self.protocol {
            Protocol::Json => self.check_json(token, kind, meta).await,
            Protocol::Headers => self.check_headers(token, meta).await,
        }
    }

    /// The header protocol: an empty `POST`; 2xx with the claims in headers, 401 and 403
    /// refuse, anything else is unavailable. The body is drained a little and ignored.
    async fn check_headers(
        &self,
        token: &str,
        meta: &RequestMeta,
    ) -> Result<Map<String, Value>, EntraError> {
        let request = self
            .request(token, meta)
            .header(CONTENT_LENGTH, "0")
            .body(Vec::new());
        let mut response = request.send().await.map_err(failure)?;
        let status = response.status();
        let answer = std::mem::take(response.headers_mut());
        let mut drained = 0;
        while drained < MAX_DRAINED_BODY {
            match response.chunk().await {
                Ok(Some(chunk)) => drained += chunk.len(),
                _ => break,
            }
        }
        drop(response);
        if status.is_success() {
            return headers::claims(
                &answer,
                &self.claim_prefix,
                self.encoded_claim_prefix.as_deref(),
            )
            .map_err(|error| match error {
                headers::ClaimError::TooLarge(limit) => unavailable(format!(
                    "the token validator's claim headers exceed a limit ({limit})"
                )),
                headers::ClaimError::Malformed => EntraError::Rejected(Reason::Malformed),
            });
        }
        let status = status.as_u16();
        if status == 401 || status == 403 {
            let error = answer
                .get_all(WWW_AUTHENTICATE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .find_map(challenge_error);
            tracing::info!(
                status,
                error = error.as_deref(),
                "the token validator refused a token"
            );
            return Err(EntraError::Rejected(Reason::Validator));
        }
        Err(unavailable(format!(
            "the token validator answered HTTP {status}"
        )))
    }

    /// The JSON protocol: `{"provider": …}` in, `{"claims": {…}}` out.
    async fn check_json(
        &self,
        token: &str,
        kind: ProviderKind,
        meta: &RequestMeta,
    ) -> Result<Map<String, Value>, EntraError> {
        let body = serde_json::json!({ "provider": kind.as_str() }).to_string();
        let request = self
            .request(token, meta)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(body);
        let mut response = request.send().await.map_err(failure)?;
        let status = response.status().as_u16();
        if status != 200 && status != 401 {
            return Err(unavailable(format!(
                "the token validator answered HTTP {status}"
            )));
        }
        let too_large = || unavailable("the token validator's answer is larger than 16 KiB");
        if response
            .content_length()
            .is_some_and(|length| length > MAX_VALIDATOR_BODY as u64)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failure)? {
            if body.len() + chunk.len() > MAX_VALIDATOR_BODY {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        if status == 401 {
            let reason = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|answer| answer.get("reason")?.as_str().and_then(Reason::from_name))
                .unwrap_or(Reason::Validator);
            return Err(EntraError::Rejected(reason));
        }
        let answer: Value = serde_json::from_slice(&body)
            .map_err(|_| unavailable("the token validator's answer is not JSON"))?;
        match answer {
            Value::Object(mut answer) => match answer.remove("claims") {
                Some(Value::Object(claims)) => Ok(claims),
                _ => Err(EntraError::Rejected(Reason::Malformed)),
            },
            _ => Err(EntraError::Rejected(Reason::Malformed)),
        }
    }
}

/// The `error` parameter of a `WWW-Authenticate` value's challenges, if it is 1 or more
/// visible ASCII characters; cut to 64. Every other parameter (the description above all)
/// is skipped.
pub(crate) fn challenge_error(challenges: &str) -> Option<String> {
    let bytes = challenges.as_bytes();
    let is_token = |byte: u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte);
    let skip = |mut at: usize, set: &[u8]| {
        while bytes.get(at).is_some_and(|byte| set.contains(byte)) {
            at += 1;
        }
        at
    };
    let token_end = |mut at: usize| {
        while bytes.get(at).copied().is_some_and(is_token) {
            at += 1;
        }
        at
    };
    let mut at = 0;
    loop {
        at = skip(at, b" \t,");
        let start = at;
        at = token_end(at);
        if at == start {
            return None;
        }
        let key = &challenges[start..at];
        at = skip(at, b" \t");
        if bytes.get(at) != Some(&b'=') {
            // An authentication scheme: its parameters follow.
            continue;
        }
        at = skip(at + 1, b" \t");
        let value = if bytes.get(at) == Some(&b'"') {
            let mut value = Vec::new();
            at += 1;
            loop {
                match *bytes.get(at)? {
                    b'"' => break,
                    b'\\' => {
                        at += 1;
                        value.push(*bytes.get(at)?);
                    }
                    byte => value.push(byte),
                }
                at += 1;
            }
            at += 1;
            value
        } else {
            let start = at;
            at = token_end(at);
            bytes[start..at].to_vec()
        };
        if key.eq_ignore_ascii_case("error") {
            if value.is_empty() || !value.iter().all(u8::is_ascii_graphic) {
                return None;
            }
            let value = String::from_utf8(value).ok()?;
            return Some(value.chars().take(MAX_CHALLENGE_ERROR_CHARS).collect());
        }
    }
}

/// `value` as a header value if it is 1 or more visible ASCII characters.
fn visible(value: &str) -> Option<HeaderValue> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return None;
    }
    HeaderValue::from_str(value).ok()
}

fn unavailable(message: impl Into<String>) -> EntraError {
    EntraError::Unavailable(message.into())
}

/// The kind of a failed request only: a `reqwest::Error` may show the URL. A request that
/// cannot be built carries a token no header can hold.
fn failure(error: reqwest::Error) -> EntraError {
    if error.is_builder() {
        // Defensive: the signature pre-check already refuses what no header can hold.
        return EntraError::Rejected(Reason::Malformed);
    }
    unavailable(if error.is_timeout() {
        "the token validator timed out"
    } else if error.is_connect() {
        "the token validator could not be reached"
    } else {
        "the token validator request failed"
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_token_that_cannot_be_sent_is_malformed() {
        let validator = Validator::new(&ValidatorSettings {
            timeout: Duration::from_millis(1000),
            ..ValidatorSettings::new(crate::entra::testing::refused_url().await)
        })
        .unwrap();
        assert_eq!(
            validator
                .check("a.b.c\u{1}d", ProviderKind::User, &RequestMeta::default())
                .await,
            Err(EntraError::Rejected(Reason::Malformed))
        );
    }

    #[test]
    fn provider_kinds_are_named_as_the_providers() {
        assert_eq!(ProviderKind::User.as_str(), "entra");
        assert_eq!(ProviderKind::App.as_str(), "entra-app");
    }
}
