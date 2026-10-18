//! The smoke test of a Microsoft Entra ID-only deployment (Plan 13B), run instead of the
//! email/password one when `--entra-token-url` names the DEV ONLY issuer's token endpoint
//! (`push-dev-entra sign-in`): the email/password routes answer 404 through the ingress;
//! two apps built on push-sdk sign in with Entra ID tokens, register, become contacts by
//! invite and chat; signing in again keeps the device. Tokens are never printed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use push_sdk::{
    Client, Config, ConnectionState, ContactState, Event, Events, MemorySecretStore, MessageStatus,
    Platform, UserId,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{STEP_TIMEOUT, Target, WORKER_TIMEOUT, step};

struct App {
    name: &'static str,
    oid: String,
    client: Client,
    events: Events,
    user: UserId,
}

/// Runs the Entra ID smoke test against `target`, with user tokens from `token_url`.
pub async fn run(target: &Target, token_url: &str) -> anyhow::Result<()> {
    let started = Instant::now();
    let http = reqwest::Client::builder()
        .timeout(STEP_TIMEOUT)
        .no_proxy()
        .build()?;
    for path in [
        "/v1/auth/login",
        "/v1/auth/signup",
        "/v1/auth/password/forgot",
    ] {
        let mut request = http
            .post(format!("{}{path}", target.auth_url.trim_end_matches('/')))
            .json(&json!({"email": "nobody@smoke.test", "password": "not a password"}));
        if let Some(host) = &target.host {
            request = request.header(reqwest::header::HOST, host);
        }
        let status = request
            .send()
            .await
            .with_context(|| format!("POST {path}"))?
            .status();
        if status != reqwest::StatusCode::NOT_FOUND {
            bail!("POST {path} answered {status}, not 404: email and password sign-in is on");
        }
    }
    step("email and password sign-in is off: login, signup and password reset answer 404");

    let mut erin = App::sign_in(target, &http, token_url, "erin").await?;
    let mut finn = App::sign_in(target, &http, token_url, "finn").await?;
    let invite = erin.client.create_invite(600, 1).await?;
    if finn.client.redeem_invite(&invite.code).await? != erin.user {
        bail!("finn's redeem named someone other than erin");
    }
    let finn_user = finn.user;
    erin.wait("the new contact", STEP_TIMEOUT, |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == finn_user && contact.state == ContactState::Accepted)
    })
    .await?;
    let chat = erin.client.open_direct(finn.user).await?.id;
    let text = format!("hello over Entra ID {}", Uuid::now_v7());
    let id = erin.client.send(chat, &text)?.client_msg_id;
    let Event::MessageAdded(received) = finn
        .wait(
            "the message",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageAdded(message) if message.client_msg_id == id),
        )
        .await?
    else {
        bail!("the wait accepts only MessageAdded");
    };
    if received.body.as_deref() != Some(text.as_str()) || received.status != MessageStatus::Received
    {
        bail!("finn's app received the wrong message");
    }
    step("erin and finn signed in with Entra ID, became contacts by invite and chatted");

    let device = erin
        .client
        .account()?
        .context("erin is signed in")?
        .device_id;
    let token = mint(&http, token_url, &erin.oid).await?;
    let again = erin
        .client
        .sign_in_with_entra(&token, None, "erin's app", Platform::Other)
        .await?;
    if again.device_id != device {
        bail!("signing in again registered another device");
    }
    step("erin signed in with Entra ID again: the same device");
    for app in [erin, finn] {
        app.client.close().await?;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

/// A user token for `oid` from the DEV ONLY issuer.
async fn mint(http: &reqwest::Client, token_url: &str, oid: &str) -> anyhow::Result<String> {
    let answer: Value = http
        .post(token_url)
        .json(&json!({ "oid": oid }))
        .send()
        .await
        .context("the DEV ONLY token endpoint")?
        .error_for_status()?
        .json()
        .await?;
    answer["token"]
        .as_str()
        .map(str::to_owned)
        .context("the token endpoint answered no token")
}

impl App {
    /// A new user `name` signs in with Entra ID (choosing a handle), registers and connects.
    async fn sign_in(
        target: &Target,
        http: &reqwest::Client,
        token_url: &str,
        name: &'static str,
    ) -> anyhow::Result<Self> {
        let oid = Uuid::now_v7().to_string();
        let suffix = &oid[oid.len() - 8..];
        let database = format!(
            "{}/{name}-entra-{suffix}.db",
            target.data_dir.trim_end_matches('/')
        );
        let mut config = Config::new(&target.auth_url, &target.socket_url, &database);
        config.host = target.host.clone();
        // The smoke test reaches the cluster's Services over plain HTTP, inside the cluster.
        config.allow_insecure_transport = true;
        let (client, events) = Client::open(config, Arc::new(MemorySecretStore::new()))?;
        let token = mint(http, token_url, &oid).await?;
        let account = client
            .sign_in_with_entra(
                &token,
                Some(&format!("{name}_{suffix}")),
                &format!("{name}'s app"),
                Platform::Other,
            )
            .await
            .context("sign in with Entra ID")?;
        client.connect()?;
        let mut app = Self {
            name,
            oid,
            client,
            events,
            user: account.user_id,
        };
        app.wait("Connected", WORKER_TIMEOUT, |event| {
            matches!(
                event,
                Event::ConnectionStateChanged(ConnectionState::Connected)
            )
        })
        .await?;
        step(&format!(
            "{name} signed in with Entra ID and connected (device {})",
            account.device_id
        ));
        Ok(app)
    }

    async fn wait(
        &mut self,
        what: &str,
        timeout: Duration,
        wanted: impl Fn(&Event) -> bool,
    ) -> anyhow::Result<Event> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.next())
                .await
                .map_err(|_| anyhow::anyhow!("{}: no {what} in time", self.name))?
                .with_context(|| format!("{}: the client is gone", self.name))?;
            if wanted(&event) {
                return Ok(event);
            }
        }
    }
}
