//! The auth part of the smoke test: a user signs up, verifies the email with the link the
//! worker's log sender delivered to the DEV ONLY mailbox, signs in, registers a device (and
//! retries the registration, which must find the same device) and refreshes its tokens, all
//! over auth-svc's public HTTP API, and checks the access token against the JWKS. Later the
//! users read each other's profiles, revoke a device and reset a password there too.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{CONNECT_TIMEOUT, Target, WORKER_TIMEOUT, step};

/// A signed-in device.
pub(crate) struct Enrolled {
    pub user: Uuid,
    pub device: Uuid,
    pub email: String,
    /// The device's current access token. Never printed.
    pub access_token: String,
}

/// The access-token claims the smoke test checks.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: Uuid,
    device_id: Uuid,
    email_verified: bool,
    aud: String,
}

/// auth-svc as a client sees it: the public API, through the ingress; and the DEV ONLY
/// mailbox, straight at the worker.
pub(crate) struct Auth<'a> {
    http: Client,
    target: &'a Target,
    jwks: JwkSet,
}

impl<'a> Auth<'a> {
    /// Waits until the public API answers (an ingress can answer 503 for a few seconds
    /// after its backends are ready) and fetches the JWKS.
    pub async fn connect(target: &'a Target, timeout: Duration) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
            .context("build the HTTP client")?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let jwks = loop {
            let answer = public(&http, target, reqwest::Method::GET, "/v1/auth/jwks")
                .send()
                .await;
            match answer {
                Ok(response) if response.status() == StatusCode::OK => {
                    break response.json::<JwkSet>().await.context("read the JWKS")?;
                }
                Ok(response) if is_transient(response.status()) && Instant::now() < deadline => {
                    step(&format!(
                        "auth-svc answers {} yet; retrying",
                        response.status()
                    ));
                }
                Ok(response) => bail!("GET /v1/auth/jwks answered {}", response.status()),
                Err(error) if Instant::now() < deadline => {
                    step(&format!(
                        "auth-svc cannot be reached yet ({}); retrying",
                        error.without_url()
                    ));
                }
                Err(error) => return Err(error.without_url()).context("GET /v1/auth/jwks"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        Ok(Self { http, target, jwks })
    }

    /// The `aud` the gateway expects: the socket URL, with the authority of the `Host`
    /// override when there is one.
    pub fn expected_audience(&self) -> String {
        audience(&self.target.socket_url, self.target.host.as_deref())
    }

    fn post(&self, path: &str, body: Value) -> RequestBuilder {
        public(&self.http, self.target, reqwest::Method::POST, path).json(&body)
    }

    /// Signs `name` up with a fresh address and handle, verifies the address with the
    /// link the worker delivered to the dev mailbox, signs in, registers a device, retries
    /// the registration with the same `registration_id` and refreshes its tokens.
    pub async fn enroll(&self, name: &str) -> anyhow::Result<Enrolled> {
        let started = Instant::now();
        let suffix = Uuid::now_v7().simple().to_string();
        let suffix = &suffix[suffix.len() - 10..];
        let email = format!("{name}.{suffix}@smoke.test");
        let handle = format!("{name}_{suffix}");
        let password = format!("smoke-{}", Uuid::now_v7());

        expect(
            self.post(
                "/v1/auth/signup",
                json!({
                    "email": email,
                    "password": password,
                    "handle": handle,
                    "display_name": name,
                }),
            ),
            StatusCode::ACCEPTED,
            "sign up",
        )
        .await?;

        let (token, waited) = self.mailbox_token(&email, "verify_email").await?;
        step(&format!(
            "{name} signed up; the worker's log sender delivered the verification email after {} ms",
            waited.as_millis()
        ));
        let page = public(
            &self.http,
            self.target,
            reqwest::Method::GET,
            "/v1/auth/verify",
        )
        .query(&[("token", token.as_str())])
        .send()
        .await
        .map_err(|error| error.without_url())
        .context("open the verification page")?;
        if page.status() != StatusCode::OK
            || page
                .headers()
                .get("referrer-policy")
                .is_none_or(|value| value != "no-referrer")
        {
            bail!(
                "the verification page answered {} without Referrer-Policy: no-referrer",
                page.status()
            );
        }
        expect(
            self.post("/v1/auth/verify", json!({"token": token})),
            StatusCode::OK,
            "verify the email",
        )
        .await?;

        let login = expect(
            self.post(
                "/v1/auth/login",
                json!({"email": email, "password": password}),
            ),
            StatusCode::OK,
            "sign in",
        )
        .await?;
        let registration = field(&login, "registration_token")?;
        // The client's idempotency key: the retry below must find the same device.
        let registration_id = Uuid::now_v7();
        let body = json!({
            "name": format!("{name}'s smoke device"),
            "platform": "other",
            "registration_id": registration_id,
        });
        let first = expect(
            public(
                &self.http,
                self.target,
                reqwest::Method::POST,
                "/v1/devices",
            )
            .bearer_auth(&registration)
            .json(&body),
            StatusCode::CREATED,
            "register a device",
        )
        .await?;
        let device = expect(
            public(
                &self.http,
                self.target,
                reqwest::Method::POST,
                "/v1/devices",
            )
            .bearer_auth(&registration)
            .json(&body),
            StatusCode::CREATED,
            "retry the registration",
        )
        .await?;
        if field(&device, "device_id")? != field(&first, "device_id")? {
            bail!("a retried registration of {name}'s device created a second device");
        }
        let user: Uuid = field(&device, "user_id")?.parse()?;
        let device_id: Uuid = field(&device, "device_id")?.parse()?;
        step(&format!(
            "{name} verified the email, signed in and registered device {device_id} (a retry returned the same device) in {} ms",
            started.elapsed().as_millis()
        ));

        let claims = self.check_access_token(&field(&device, "access_token")?)?;
        if claims.sub != user || claims.device_id != device_id || !claims.email_verified {
            bail!("{name}'s access token has the wrong claims: {claims:?}");
        }
        let refreshed = expect(
            self.post(
                "/v1/auth/refresh",
                json!({"refresh_token": field(&device, "refresh_token")?}),
            ),
            StatusCode::OK,
            "refresh the tokens",
        )
        .await?;
        if field(&refreshed, "device_id")? != device_id.to_string() {
            bail!("refreshing returned another device than {name}'s");
        }
        let renewed = self.check_access_token(&field(&refreshed, "access_token")?)?;
        if renewed.sub != user || renewed.device_id != device_id {
            bail!("{name}'s refreshed access token has the wrong claims: {renewed:?}");
        }
        step(&format!(
            "{name}'s access token verifies against the JWKS (EdDSA, aud {}); refresh rotated it",
            claims.aud
        ));
        Ok(Enrolled {
            user,
            device: device_id,
            email,
            access_token: field(&refreshed, "access_token")?,
        })
    }

    /// The user ids among `ids` whose profiles `GET /v1/users?ids=…` shows the holder of
    /// `access_token`; every profile shown must have a handle.
    pub async fn profiles(&self, access_token: &str, ids: &[Uuid]) -> anyhow::Result<Vec<Uuid>> {
        let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        let answer = expect(
            public(&self.http, self.target, reqwest::Method::GET, "/v1/users")
                .query(&[("ids", ids.join(","))])
                .bearer_auth(access_token),
            StatusCode::OK,
            "read profiles",
        )
        .await?;
        let users = answer["users"]
            .as_array()
            .context("the profiles answer has no users")?;
        users
            .iter()
            .map(|user| {
                field(user, "handle")?;
                Ok(field(user, "user_id")?.parse()?)
            })
            .collect()
    }

    /// Revokes `device` with its own access token, as a user signing a device out does.
    pub async fn revoke_device(&self, access_token: &str, device: Uuid) -> anyhow::Result<()> {
        let path = format!("/v1/devices/{device}");
        let response = public(&self.http, self.target, reqwest::Method::DELETE, &path)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|error| error.without_url())
            .context("revoke a device: request failed")?;
        if response.status() != StatusCode::NO_CONTENT {
            bail!("revoke a device: answered {}", response.status());
        }
        Ok(())
    }

    /// Resets the password of `email` through the link the worker delivers.
    pub async fn reset_password(&self, email: &str) -> anyhow::Result<()> {
        expect(
            self.post("/v1/auth/password/forgot", json!({"email": email})),
            StatusCode::ACCEPTED,
            "ask for a password reset",
        )
        .await?;
        self.complete_password_reset(email).await?;
        Ok(())
    }

    /// Opens the newest reset link the worker delivered to `email` and sets a new
    /// password, as the user does in a browser; the new password.
    pub async fn complete_password_reset(&self, email: &str) -> anyhow::Result<String> {
        let (token, _) = self.mailbox_token(email, "reset_password").await?;
        let password = format!("smoke-{}", Uuid::now_v7());
        expect(
            self.post(
                "/v1/auth/password/reset",
                json!({"token": token, "password": password}),
            ),
            StatusCode::OK,
            "reset the password",
        )
        .await?;
        Ok(password)
    }

    /// Verifies `email` with the link the worker delivered, as the user does in a
    /// browser (an app signed up through the SDK).
    pub async fn verify_email(&self, email: &str) -> anyhow::Result<()> {
        let (token, _) = self.mailbox_token(email, "verify_email").await?;
        expect(
            self.post("/v1/auth/verify", json!({"token": token})),
            StatusCode::OK,
            "verify the email",
        )
        .await?;
        Ok(())
    }

    /// The token of the link in the newest `template` email to `email`, and how long it
    /// took to come: the worker relays the `email.send` event from auth-svc's outbox and
    /// its log sender drops the email into the dev mailbox, which is polled until it
    /// arrives.
    async fn mailbox_token(
        &self,
        email: &str,
        template: &str,
    ) -> anyhow::Result<(String, Duration)> {
        let url = format!(
            "{}/dev/mailbox",
            self.target.mailbox_url.trim_end_matches('/')
        );
        let started = Instant::now();
        let text = loop {
            let mailbox: Value = self
                .http
                .get(&url)
                .query(&[("to", email)])
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(|error| error.without_url())
                .with_context(|| format!("read the dev mailbox at {url}"))?
                .json()
                .await
                .map_err(|error| error.without_url())
                .context("read the dev mailbox")?;
            let text = mailbox["emails"].as_array().and_then(|emails| {
                emails
                    .iter()
                    .rev()
                    .find(|email| email["template"] == template)
                    .and_then(|email| email["text"].as_str().map(str::to_owned))
            });
            match text {
                Some(text) => break text,
                None if started.elapsed() < WORKER_TIMEOUT => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                None => bail!(
                    "no {template} email for {email} in the dev mailbox within {WORKER_TIMEOUT:?}"
                ),
            }
        };
        let link = text
            .lines()
            .find(|line| line.starts_with("http"))
            .with_context(|| format!("the {template} email has no link"))?;
        let token = Url::parse(link)
            .map_err(|_| anyhow::anyhow!("the {template} link is not a URL"))?
            .query_pairs()
            .find(|(key, _)| key == "token")
            .map(|(_, token)| token.into_owned())
            .with_context(|| format!("the {template} link has no token"))?;
        Ok((token, started.elapsed()))
    }

    /// Checks an access token's signature against the JWKS, as the gateway will.
    fn check_access_token(&self, token: &str) -> anyhow::Result<Claims> {
        let header = jsonwebtoken::decode_header(token).context("decode the token header")?;
        let kid = header.kid.context("the token has no kid")?;
        let jwk = self
            .jwks
            .find(&kid)
            .with_context(|| format!("the JWKS has no key {kid}"))?;
        let key = DecodingKey::from_jwk(jwk).context("load the JWK")?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[self.expected_audience()]);
        let data = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .context("the access token does not verify against the JWKS")?;
        Ok(data.claims)
    }
}

/// A request to the public API: through the ingress, with its host as `Host`.
fn public(http: &Client, target: &Target, method: reqwest::Method, path: &str) -> RequestBuilder {
    let url = format!("{}{path}", target.auth_url.trim_end_matches('/'));
    let request = http.request(method, url);
    match &target.host {
        Some(host) => request.header(reqwest::header::HOST, host),
        None => request,
    }
}

/// Sends `request`; its JSON body if the status is `expected`.
async fn expect(
    request: RequestBuilder,
    expected: StatusCode,
    what: &str,
) -> anyhow::Result<Value> {
    let response = request
        .send()
        .await
        .map_err(|error| error.without_url())
        .with_context(|| format!("{what}: request failed"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status != expected {
        bail!("{what}: {}", unexpected_status(expected, status, &body));
    }
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// Describes an unexpected status without the body, which can hold tokens: only the
/// API's error code and message are quoted.
fn unexpected_status(expected: StatusCode, status: StatusCode, body: &str) -> String {
    let error = serde_json::from_str::<Value>(body)
        .ok()
        .map(|body| body["error"].clone());
    let quote = |name: &str| {
        error
            .as_ref()
            .and_then(|error| error[name].as_str().map(str::to_owned))
    };
    match (quote("code"), quote("message")) {
        (Some(code), Some(message)) => {
            format!("expected {expected}, got {status} ({code}: {message})")
        }
        (Some(code), None) => format!("expected {expected}, got {status} ({code})"),
        _ => format!("expected {expected}, got {status}"),
    }
}

/// The statuses of a gateway that is not ready yet.
fn is_transient(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

/// The `aud` of access tokens: `socket_url`, with the authority of `host` when set.
fn audience(socket_url: &str, host: Option<&str>) -> String {
    let Some(host) = host else {
        return socket_url.to_owned();
    };
    match socket_url.split_once("://") {
        Some((scheme, rest)) => {
            let path = rest.find('/').map_or("", |at| &rest[at..]);
            format!("{scheme}://{host}{path}")
        }
        None => socket_url.to_owned(),
    }
}

fn field(value: &Value, name: &str) -> anyhow::Result<String> {
    value[name]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("the response has no {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audience_takes_the_authority_of_the_host_override() {
        assert_eq!(
            audience("ws://192.168.49.2:80/v1/socket", Some("push.local")),
            "ws://push.local/v1/socket"
        );
        assert_eq!(
            audience("ws://127.0.0.1:5000/v1/socket", None),
            "ws://127.0.0.1:5000/v1/socket"
        );
    }

    #[test]
    fn errors_do_not_quote_tokens() {
        let secret = "eyJhbGciOiJFZERTQSJ9.secret-token";
        let value = json!({"refresh_token": secret});
        let missing = field(&value, "access_token").unwrap_err().to_string();
        assert!(missing.contains("access_token") && !missing.contains(secret));

        let body = json!({"registration_token": secret}).to_string();
        let plain = unexpected_status(StatusCode::OK, StatusCode::BAD_REQUEST, &body);
        assert!(plain.contains("400") && !plain.contains(secret));

        let body = json!({"error": {"code": "bad_token", "message": "expired"}, "token": secret})
            .to_string();
        let shaped = unexpected_status(StatusCode::OK, StatusCode::UNAUTHORIZED, &body);
        assert!(shaped.contains("bad_token") && shaped.contains("expired"));
        assert!(!shaped.contains(secret));
    }

    #[test]
    fn only_gateway_errors_are_retried() {
        assert!(is_transient(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_transient(StatusCode::NOT_FOUND));
        assert!(!is_transient(StatusCode::INTERNAL_SERVER_ERROR));
    }
}
