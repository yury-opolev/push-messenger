//! The auth part of the smoke test: a user signs up, verifies the email through the
//! DEV ONLY mailbox, signs in, registers a device (and retries the registration, which
//! must find the same device) and refreshes its tokens, all over auth-svc's public HTTP
//! API, and checks the access token against the JWKS.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{CONNECT_TIMEOUT, Target, step};

/// A signed-in device.
pub(crate) struct Enrolled {
    pub user: Uuid,
    pub device: Uuid,
}

/// The access-token claims the smoke test checks.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: Uuid,
    device_id: Uuid,
    email_verified: bool,
    aud: String,
}

/// auth-svc as a client sees it: the public API, through the ingress, and the DEV ONLY
/// mailbox, straight at the service.
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
                Ok(response) if Instant::now() < deadline => {
                    step(&format!(
                        "auth-svc answers {} yet; retrying",
                        response.status()
                    ));
                }
                Err(error) if Instant::now() < deadline => {
                    step(&format!(
                        "auth-svc cannot be reached yet ({error}); retrying"
                    ));
                }
                Ok(response) => bail!("GET /v1/auth/jwks answered {}", response.status()),
                Err(error) => return Err(error).context("GET /v1/auth/jwks"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        Ok(Self { http, target, jwks })
    }

    fn post(&self, path: &str, body: Value) -> RequestBuilder {
        public(&self.http, self.target, reqwest::Method::POST, path).json(&body)
    }

    /// Signs `name` up with a fresh address and handle, verifies the address with the
    /// link from the dev mailbox, signs in, registers a device, retries the registration
    /// with the same `registration_id` and refreshes its tokens.
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

        let token = self.verification_token(&email).await?;
        let page = public(
            &self.http,
            self.target,
            reqwest::Method::GET,
            &format!("/v1/auth/verify?token={token}"),
        )
        .send()
        .await
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
            "{name} signed up, verified the email from the dev mailbox, signed in and registered device {device_id} (a retry returned the same device) in {} ms",
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
            bail!("refreshing returned another device: {refreshed}");
        }
        self.check_access_token(&field(&refreshed, "access_token")?)?;
        step(&format!(
            "{name}'s access token verifies against the JWKS (EdDSA, aud {}); refresh rotated it",
            claims.aud
        ));
        Ok(Enrolled {
            user,
            device: device_id,
        })
    }

    /// The token of the newest verification link sent to `email`, from the dev mailbox.
    async fn verification_token(&self, email: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/dev/mailbox?to={email}",
            self.target.mailbox_url.trim_end_matches('/')
        );
        let mailbox: Value = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(Response::error_for_status)
            .with_context(|| format!("read the dev mailbox at {url}"))?
            .json()
            .await?;
        let link = mailbox["emails"]
            .as_array()
            .and_then(|emails| {
                emails
                    .iter()
                    .find(|email| email["template"] == "verify_email")
            })
            .and_then(|email| email["params"]["link"].as_str())
            .with_context(|| format!("no verification email for {email}: {mailbox}"))?;
        let (_, token) = link
            .split_once("?token=")
            .with_context(|| format!("a verification link without a token: {link}"))?;
        Ok(token.to_owned())
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
        // The audience is the gateway's URL, which the smoke test does not know; it
        // checks the signature and the identity claims.
        validation.validate_aud = false;
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
        .with_context(|| format!("{what}: request failed"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status != expected {
        bail!("{what}: expected {expected}, got {status}: {body}");
    }
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn field(value: &Value, name: &str) -> anyhow::Result<String> {
    value[name]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("no {name} in {value}"))
}
