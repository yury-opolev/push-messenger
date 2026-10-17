//! The SDK half of the smoke test: two apps built on `push-sdk`, as a third party builds
//! them, against the same deployment and through the same ingress.
//!
//! Dana and Erin sign up and in with the SDK (the gateway answers 4006 until the worker
//! relays a device, and the SDK reconnects by itself), become contacts by invite and chat:
//! a message, its delivery and read receipt, an edit and a delete. Erin queues messages
//! offline and restarts her app: the outbox sends them after the restart, in order and
//! once, and a send cut off by a restart is stored once; Erin's database is encrypted with
//! a key derived from a passphrase, and its file holds none of her messages' text. A copy
//! of Dana's app opened with her credentials takes over her device's connection: the first
//! app reports `OpenedElsewhere` (4007). Finally the copy revokes the device and Erin
//! resets her password: the SDK reports `DeviceRemoved` (4003) and `SignedOut` (4001), and
//! Erin signs in again with the same device and her history.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use push_sdk::{
    Client, ClientMsgId, Config, ConnectionState, ContactState, ConversationId, DatabaseKey, Event,
    Events, MemorySecretStore, MessageStatus, Platform, SECRET_KEYS, SecretStore, UserId,
};
use uuid::Uuid;

use crate::auth::Auth;
use crate::{STEP_TIMEOUT, Target, WORKER_TIMEOUT};

/// One app: a client with its own database and secret store.
struct App {
    name: &'static str,
    email: String,
    password: String,
    user: UserId,
    database: String,
    secrets: Arc<MemorySecretStore>,
    config: Config,
    client: Client,
    events: Events,
    /// Events received while the test waited for another.
    pending: VecDeque<Event>,
}

impl App {
    /// Signs `name` up with the SDK, verifies the address with the link the worker
    /// delivered, signs in (registering the device) and connects. `encrypted`: the
    /// database is encrypted with a key derived from a passphrase, as an app asking for a
    /// PIN would.
    async fn enroll(
        target: &Target,
        auth: &Auth<'_>,
        name: &'static str,
        encrypted: bool,
    ) -> anyhow::Result<Self> {
        let started = Instant::now();
        let suffix = Uuid::now_v7().simple().to_string();
        let suffix = &suffix[suffix.len() - 10..];
        let email = format!("{name}.{suffix}@smoke.test");
        let password = format!("smoke-{}", Uuid::now_v7());
        let database = format!(
            "{}/{name}-{suffix}.db",
            target.data_dir.trim_end_matches('/')
        );
        let mut config = Config::new(&target.auth_url, &target.socket_url, &database);
        config.host = target.host.clone();
        // The smoke test reaches the cluster's Services over plain HTTP, inside the cluster.
        config.allow_insecure_transport = true;
        if encrypted {
            let passphrase = format!("{name}'s passphrase {suffix}");
            let salt = DatabaseKey::new_salt();
            config.database_key = Some(DatabaseKey::derive(passphrase.as_bytes(), &salt)?);
        }
        let secrets = Arc::new(MemorySecretStore::new());
        let (client, events) = Client::open(config.clone(), secrets.clone())?;
        client
            .sign_up(&email, &password, &format!("{name}_{suffix}"), name)
            .await
            .context("sign up with the SDK")?;
        auth.verify_email(&email).await?;
        let account = client
            .sign_in(&email, &password, &format!("{name}'s app"), Platform::Other)
            .await
            .context("sign in with the SDK")?;
        let mut app = Self {
            name,
            email,
            password,
            user: account.user_id,
            database,
            secrets,
            config,
            client,
            events,
            pending: VecDeque::new(),
        };
        let retries = app.connect().await?;
        sdk_step(&format!(
            "{name} signed up and in with push-sdk and connected (device {}, {retries} reconnects while the device was not relayed yet) in {} ms",
            account.device_id,
            started.elapsed().as_millis()
        ));
        Ok(app)
    }

    /// Connects and waits for `Connected`; how many times the SDK had to retry.
    async fn connect(&mut self) -> anyhow::Result<usize> {
        self.client.connect()?;
        let mut retries = 0;
        let deadline = tokio::time::Instant::now() + WORKER_TIMEOUT;
        loop {
            let event = self.next_event(deadline, "Connected").await?;
            match event {
                Event::ConnectionStateChanged(ConnectionState::Connected) => return Ok(retries),
                Event::ConnectionStateChanged(ConnectionState::Reconnecting { .. }) => retries += 1,
                Event::ConnectionStateChanged(
                    state @ (ConnectionState::SignedOut
                    | ConnectionState::DeviceRemoved
                    | ConnectionState::OpenedElsewhere),
                ) => bail!("{}: {state:?} while connecting", self.name),
                other => self.pending.push_back(other),
            }
        }
    }

    async fn next_event(
        &mut self,
        deadline: tokio::time::Instant,
        waiting_for: &str,
    ) -> anyhow::Result<Event> {
        tokio::time::timeout_at(deadline, self.events.next())
            .await
            .map_err(|_| anyhow::anyhow!("{}: no {waiting_for} in time", self.name))?
            .with_context(|| format!("{}: the client is gone", self.name))
    }

    /// The first event `wanted` accepts, kept or new, within `timeout`; the others are
    /// kept. A `SendFailed` that no wait accepts fails the test.
    async fn wait_for(
        &mut self,
        waiting_for: &str,
        timeout: Duration,
        wanted: impl Fn(&Event) -> bool,
    ) -> anyhow::Result<Event> {
        if let Some(index) = self.pending.iter().position(&wanted) {
            return Ok(self.pending.remove(index).expect("in range"));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = self.next_event(deadline, waiting_for).await?;
            if wanted(&event) {
                return Ok(event);
            }
            if let Event::SendFailed { error, .. } = &event {
                bail!(
                    "{}: a send failed while waiting for {waiting_for}: {}",
                    self.name,
                    error.code()
                );
            }
            self.pending.push_back(event);
        }
    }

    /// Ends the app as a process exit would, and starts it again on the same database
    /// and secret store.
    fn restart(&mut self) -> anyhow::Result<()> {
        let (client, events) = Client::open(self.config.clone(), self.secrets.clone())?;
        self.client = client;
        self.events = events;
        self.pending.clear();
        Ok(())
    }
}

/// Runs the SDK scenario.
pub(crate) async fn run(target: &Target, auth: &Auth<'_>) -> anyhow::Result<()> {
    let mut dana = App::enroll(target, auth, "dana", false).await?;
    let mut erin = App::enroll(target, auth, "erin", true).await?;

    let invite = dana.client.create_invite(600, 1).await?;
    let creator = erin.client.redeem_invite(&invite.code).await?;
    if creator != dana.user {
        bail!("erin's redeem named someone other than dana");
    }
    let erin_user = erin.user;
    dana.wait_for("the new contact", STEP_TIMEOUT, |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == erin_user && contact.state == ContactState::Accepted)
    })
    .await?;
    sdk_step("erin redeemed dana's invite; dana's app heard the new contact");

    let chat = dana.client.open_direct(erin.user).await?.id;
    let (seq, sent_id) = chat_and_read(&mut dana, &mut erin, chat).await?;
    edit_and_delete(&mut dana, &mut erin, chat, sent_id).await?;
    let next = restart_resumes_the_outbox(&mut dana, &mut erin, chat, seq).await?;
    cut_off_send_is_stored_once(&mut dana, &mut erin, chat, next).await?;
    let mut copy = opened_elsewhere(target, &mut dana).await?;
    removed_and_signed_out(auth, &mut copy, &mut erin, chat).await?;
    let _ = (dana.database, erin.database, dana.password, copy.password);
    Ok(())
}

/// Whether the database file or its write-ahead log holds `text`.
fn file_holds(database: &str, text: &str) -> anyhow::Result<bool> {
    let mut found = false;
    for path in [database.to_owned(), format!("{database}-wal")] {
        match std::fs::read(&path) {
            Ok(bytes) => {
                found |= bytes
                    .windows(text.len())
                    .any(|window| window == text.as_bytes());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read the database file"),
        }
    }
    Ok(found)
}

/// Dana sends, Erin's app receives and reads, Dana's app shows it read.
async fn chat_and_read(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
) -> anyhow::Result<(i64, ClientMsgId)> {
    let started = Instant::now();
    let text = format!("hello from push-sdk {}", Uuid::now_v7());
    let echo = dana.client.send(chat, &text)?;
    if echo.status != MessageStatus::Pending {
        bail!("the local echo is {:?}, not Pending", echo.status);
    }
    let id = echo.client_msg_id;
    let Event::MessageSent(sent) = dana
        .wait_for(
            "dana's MessageSent",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageSent(message) if message.client_msg_id == id),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageSent");
    };
    let seq = sent.seq.context("a sent message has a seq")?;
    let Event::MessageAdded(received) = erin
        .wait_for(
            "erin's MessageAdded",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageAdded(message) if message.client_msg_id == id),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    if received.body.as_deref() != Some(text.as_str()) || received.status != MessageStatus::Received
    {
        bail!(
            "erin's app received the wrong message (seq {:?})",
            received.seq
        );
    }
    sdk_step(&format!(
        "dana's message was stored (seq {seq}) and erin's app received it after {} ms",
        started.elapsed().as_millis()
    ));

    erin.client.mark_read(chat, seq)?;
    dana.wait_for("the read receipt", STEP_TIMEOUT, |event| {
        matches!(event, Event::ReceiptChanged { conversation_id, peer_read_seq, .. }
            if *conversation_id == chat && *peer_read_seq >= seq)
    })
    .await?;
    let status = dana
        .client
        .messages(chat, None, 50)?
        .into_iter()
        .find(|message| message.client_msg_id == id)
        .map(|message| message.status);
    if status != Some(MessageStatus::Read) {
        bail!("dana's app shows the message as {status:?}, not Read");
    }
    sdk_step("erin read it; dana's app shows it read");
    Ok((seq, id))
}

async fn edit_and_delete(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
    id: ClientMsgId,
) -> anyhow::Result<()> {
    dana.client.edit(chat, id, "hello, edited")?;
    erin.wait_for("erin's MessageEdited", STEP_TIMEOUT, |event| {
        matches!(event, Event::MessageEdited(message)
            if message.client_msg_id == id && message.body.as_deref() == Some("hello, edited"))
    })
    .await?;
    dana.client.delete(chat, id)?;
    erin.wait_for(
        "erin's MessageDeleted",
        STEP_TIMEOUT,
        |event| matches!(event, Event::MessageDeleted(message) if message.client_msg_id == id),
    )
    .await?;
    let held = erin
        .client
        .messages(chat, None, 50)?
        .into_iter()
        .find(|message| message.client_msg_id == id)
        .context("erin's app no longer holds the message")?;
    if !held.deleted || held.body.is_some() {
        bail!("erin's app still holds the deleted message's body");
    }
    sdk_step("dana edited and deleted it; erin's app applied both and holds no body");
    Ok(())
}

/// Erin queues three messages offline and restarts her app; once connected the outbox
/// sends them in order. The `seq` the next message gets.
async fn restart_resumes_the_outbox(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
    after: i64,
) -> anyhow::Result<i64> {
    erin.client.disconnect()?;
    erin.wait_for("Offline", STEP_TIMEOUT, |event| {
        *event == Event::ConnectionStateChanged(ConnectionState::Offline)
    })
    .await?;
    let mut queued = Vec::new();
    for n in 1..=3 {
        queued.push(
            erin.client
                .send(chat, &format!("queued offline {n}"))?
                .client_msg_id,
        );
    }
    erin.restart()?;
    let pending = erin
        .client
        .messages(chat, None, 50)?
        .iter()
        .filter(|message| message.status == MessageStatus::Pending)
        .count();
    if pending != 3 {
        bail!("after the restart erin's outbox holds {pending} messages, not 3");
    }
    if file_holds(&erin.database, "queued offline")? {
        bail!("erin's encrypted database file holds a message's text");
    }
    erin.connect().await?;
    let mut seqs = Vec::new();
    for id in &queued {
        let Event::MessageAdded(message) = dana
            .wait_for("a queued message", STEP_TIMEOUT, |event| {
                matches!(event, Event::MessageAdded(message) if message.client_msg_id == *id)
            })
            .await?
        else {
            unreachable!("the wait accepts only MessageAdded");
        };
        seqs.push(message.seq.context("a delivered message has a seq")?);
    }
    // Positive control: the same scan finds the text in a plain database.
    if !file_holds(&dana.database, "queued offline")? {
        bail!("the scan did not find the text in dana's plain database");
    }
    // The edit and the delete took after + 1 and after + 2.
    let first = after + 3;
    if seqs != [first, first + 1, first + 2] {
        bail!("the queued messages arrived as {seqs:?}, not in order from {first}");
    }
    for id in &queued {
        erin.wait_for(
            "erin's MessageSent",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageSent(message) if message.client_msg_id == *id),
        )
        .await?;
    }
    sdk_step(&format!(
        "erin queued 3 messages offline and restarted her app (her database encrypted: the file holds none of their text); once connected the outbox sent them in order (seq {}..{}) and dana's app received each once",
        seqs[0], seqs[2]
    ));
    Ok(first + 3)
}

/// A send whose app ends at once is stored once — retried after the restart with the same
/// `client_msg_id`, or acknowledged before the old app stopped: the following message
/// takes the next `seq`.
async fn cut_off_send_is_stored_once(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
    expected: i64,
) -> anyhow::Result<()> {
    let id = erin
        .client
        .send(chat, "cut off by a restart")?
        .client_msg_id;
    erin.restart()?;
    erin.connect().await?;
    // A dropped client's runtime stops at its next await, not at once: when the first
    // attempt was answered in that moment, the old app recorded the SendAck and the
    // restarted one finds the message sent already. Otherwise it sends it again.
    let held = erin
        .client
        .messages(chat, None, 50)?
        .into_iter()
        .find(|message| message.client_msg_id == id)
        .and_then(|message| message.seq);
    let (sent_seq, how) = match held {
        Some(seq) => (
            Some(seq),
            "its first attempt was acknowledged before the app stopped",
        ),
        None => {
            let Event::MessageSent(sent) = erin
                .wait_for(
                    "the retried MessageSent",
                    STEP_TIMEOUT,
                    |event| matches!(event, Event::MessageSent(message) if message.client_msg_id == id),
                )
                .await?
            else {
                unreachable!("the wait accepts only MessageSent");
            };
            (sent.seq, "retried with its client_msg_id")
        }
    };
    let next = erin
        .client
        .send(chat, "after the cut-off send")?
        .client_msg_id;
    let Event::MessageAdded(following) = dana
        .wait_for(
            "the following message",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageAdded(message) if message.client_msg_id == next),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    if sent_seq != Some(expected) || following.seq != Some(expected + 1) {
        bail!(
            "the cut-off send took {sent_seq:?} and the next {:?}, not {expected} and {}",
            following.seq,
            expected + 1
        );
    }
    sdk_step(&format!(
        "a send cut off by a restart was stored once ({how}; seq {expected}, the next took {})",
        expected + 1
    ));
    Ok(())
}

/// Dana's credentials are copied into a second app with its own database, as a backup
/// restored on another machine would: when the copy connects, the gateway lets only the
/// newer connection stay, and Dana's first app reports `OpenedElsewhere` (4007) and stops.
/// The copy, which holds the device now.
async fn opened_elsewhere(target: &Target, dana: &mut App) -> anyhow::Result<App> {
    let started = Instant::now();
    let secrets = Arc::new(MemorySecretStore::new());
    for key in SECRET_KEYS {
        if let Some(value) = dana.secrets.load(key)? {
            secrets.save(key, &value)?;
        }
    }
    let database = format!("{}-copy.db", dana.database.trim_end_matches(".db"));
    let mut config = Config::new(&target.auth_url, &target.socket_url, &database);
    config.host = target.host.clone();
    config.allow_insecure_transport = true;
    let (client, events) = Client::open(config.clone(), secrets.clone())?;
    let mut copy = App {
        name: "dana's copy",
        email: dana.email.clone(),
        password: dana.password.clone(),
        user: dana.user,
        database,
        secrets,
        config,
        client,
        events,
        pending: VecDeque::new(),
    };
    copy.connect().await?;
    dana.wait_for("OpenedElsewhere", STEP_TIMEOUT, |event| {
        *event == Event::OpenedElsewhere
    })
    .await?;
    if dana.client.connection_state() != ConnectionState::OpenedElsewhere {
        bail!(
            "dana's app is {:?}, not OpenedElsewhere",
            dana.client.connection_state()
        );
    }
    sdk_step(&format!(
        "a copy of dana's app with her credentials connected; her first app reported OpenedElsewhere (4007) after {} ms and stopped",
        started.elapsed().as_millis()
    ));
    Ok(copy)
}

/// The copy of Dana's app revokes her device and Erin resets her password: the SDK
/// reports `DeviceRemoved` and `SignedOut`, and Erin signs in again with the same device.
async fn removed_and_signed_out(
    auth: &Auth<'_>,
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let device = dana
        .client
        .account()?
        .context("dana is signed in")?
        .device_id;
    dana.client.revoke_device(device).await?;
    dana.wait_for("DeviceRemoved", WORKER_TIMEOUT, |event| {
        *event == Event::DeviceRemoved
    })
    .await?;
    if dana.client.connection_state() != ConnectionState::DeviceRemoved {
        bail!(
            "{} is {:?}, not DeviceRemoved",
            dana.name,
            dana.client.connection_state()
        );
    }
    sdk_step(&format!(
        "the copy revoked dana's device; it reported DeviceRemoved (4003) after {} ms and stopped",
        started.elapsed().as_millis()
    ));

    let started = Instant::now();
    let device = erin
        .client
        .account()?
        .context("erin is signed in")?
        .device_id;
    let held_before = erin.client.messages(chat, None, 50)?.len();
    erin.client.forgot_password(&erin.email).await?;
    erin.password = auth.complete_password_reset(&erin.email).await?;
    erin.wait_for("SignedOut", WORKER_TIMEOUT, |event| {
        *event == Event::SignedOut
    })
    .await?;
    let waited = started.elapsed();
    if erin.client.account()?.is_some() {
        bail!("erin's app still holds credentials after SignedOut");
    }
    // The gateway refuses a token issued in the same second as the reset (spec §5.3:
    // within that second the order is unknown), so a person signing in again within a
    // second is signed out once more. A person takes longer; the test waits.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let account = erin
        .client
        .sign_in(&erin.email, &erin.password, "erin's app", Platform::Other)
        .await?;
    if account.device_id != device {
        bail!("erin's app registered a new device instead of signing the same one in");
    }
    erin.connect().await?;
    let history = erin.client.messages(chat, None, 50)?.len();
    if history == 0 || history != held_before {
        bail!("erin's app held {held_before} messages before the reset and {history} after");
    }
    sdk_step(&format!(
        "erin reset her password; her app reported SignedOut (4001) after {} ms, signed the same device in again and kept its {history} messages",
        waited.as_millis()
    ));
    erin.client.disconnect()?;
    Ok(())
}

fn sdk_step(message: &str) {
    println!("smoke: sdk: {message}");
}
