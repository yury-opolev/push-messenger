//! Preparation, through the public API only: signs in the operator's accounts, registers
//! their devices and makes sample pairs contacts with a direct conversation. Resumable:
//! the state is saved after every change and before every request that could create
//! something, so an interrupted preparation continues where it stopped.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use push_proto::v1::{CreateInvite, OpenDirect, RedeemInvite, request, response};
use push_proto::{id, id_bytes};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use uuid::Uuid;

use crate::accounts::Account;
use crate::config::{MAX_CONNECTIONS, PrepareOptions, Target};
use crate::http::{ApiError, AuthApi, DeviceTokens};
use crate::socket::{self, Opened, Ready, Socket};
use crate::state::{AccountState, DeviceRef, DeviceState, Pair, State};

/// How long a newly registered device may take to reach the gateway (the worker relays it).
const READY_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareSummary {
    pub accounts: usize,
    pub devices_ready: usize,
    pub devices_wanted: usize,
    pub pairs_ready: usize,
    pub pairs_wanted: usize,
    /// `accounts line N: reason`, in the accounts file's order.
    pub failures: Vec<String>,
}

impl PrepareSummary {
    pub fn complete(&self) -> bool {
        self.failures.is_empty()
            && self.devices_ready >= self.devices_wanted
            && self.pairs_ready >= self.pairs_wanted
    }
}

/// The state, shared by the workers; every change is saved at once.
#[derive(Clone)]
struct Shared {
    state: Arc<Mutex<State>>,
    path: Arc<Path>,
}

impl Shared {
    async fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> anyhow::Result<T> {
        let mut state = self.state.lock().await;
        let result = change(&mut state);
        state.save(&self.path)?;
        Ok(result)
    }

    async fn read<T>(&self, look: impl FnOnce(&State) -> T) -> T {
        look(&*self.state.lock().await)
    }
}

/// Why an account could not be prepared, or why preparation must stop.
enum Failure {
    Account(String),
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

fn classify(error: ApiError) -> Failure {
    match error {
        ApiError::Rejected { status: 401, .. } => {
            Failure::Account("invalid_credentials: wrong email or password".to_owned())
        }
        ApiError::Rejected { status: 403, .. } => {
            Failure::Account("email_not_verified: verify the account before using it".to_owned())
        }
        ApiError::Rejected { status: 409, .. } => Failure::Account(
            "too_many_devices: the account has other active devices; use dedicated test accounts"
                .to_owned(),
        ),
        ApiError::Rejected { status, code } => Failure::Account(format!("{code} (HTTP {status})")),
        other => Failure::Fatal(anyhow::anyhow!("the target could not be used: {other}")),
    }
}

pub async fn prepare(
    target: &Target,
    accounts: &[Account],
    state_path: &Path,
    options: &PrepareOptions,
) -> anyhow::Result<PrepareSummary> {
    options.validate()?;
    crate::state::ensure_outside_git(state_path)?;
    if options.sample_pairs * 2 > accounts.len() {
        bail!(
            "--sample-pairs {} needs at least {} accounts",
            options.sample_pairs,
            options.sample_pairs * 2
        );
    }
    if accounts.len() * options.devices_per_account > MAX_CONNECTIONS {
        bail!("at most {MAX_CONNECTIONS} devices: use fewer accounts or devices per account");
    }
    let mut state = State::load(state_path)?.unwrap_or_else(|| State::new(target.authority()));
    if state.target != target.authority() {
        bail!(
            "{} was prepared for {}, not {}",
            state_path.display(),
            state.target,
            target.authority()
        );
    }
    let mut indices = Vec::with_capacity(accounts.len());
    for account in accounts {
        let email = account.email.trim();
        let index = match state
            .accounts
            .iter()
            .position(|known| known.email.eq_ignore_ascii_case(email))
        {
            Some(index) => index,
            None => {
                state.accounts.push(AccountState::new(email));
                state.accounts.len() - 1
            }
        };
        state.accounts[index].failure = None;
        indices.push(index);
    }
    state.save(state_path)?;

    let api = AuthApi::new(target)?;
    let shared = Shared {
        state: Arc::new(Mutex::new(state)),
        path: Arc::from(state_path),
    };
    let limit = Arc::new(Semaphore::new(options.concurrency));
    let mut workers = JoinSet::new();
    for (account, index) in accounts.iter().cloned().zip(indices.iter().copied()) {
        let (api, shared, limit) = (api.clone(), shared.clone(), limit.clone());
        let wanted = options.devices_per_account;
        workers.spawn(async move {
            let _permit = limit
                .acquire_owned()
                .await
                .expect("the semaphore stays open");
            match prepare_account(&api, &shared, index, &account, wanted).await {
                Ok(()) => Ok(()),
                Err(Failure::Account(reason)) => {
                    shared
                        .update(|state| state.accounts[index].failure = Some(reason))
                        .await
                }
                Err(Failure::Fatal(error)) => Err(error),
            }
        });
    }
    while let Some(joined) = workers.join_next().await {
        joined.context("a preparation worker panicked")??;
    }
    let pairs_ready = prepare_pairs(&api, target, &shared, &indices, options.sample_pairs).await?;
    let state = shared.state.lock().await;
    let listed = || indices.iter().map(|&index| &state.accounts[index]);
    Ok(PrepareSummary {
        accounts: indices.len(),
        devices_ready: listed()
            .map(|account| {
                account
                    .devices
                    .iter()
                    .filter(|device| device.is_ready())
                    .count()
                    .min(options.devices_per_account)
            })
            .sum(),
        devices_wanted: indices.len() * options.devices_per_account,
        pairs_ready,
        pairs_wanted: options.sample_pairs,
        failures: listed()
            .enumerate()
            .filter_map(|(line, account)| {
                account
                    .failure
                    .as_ref()
                    .map(|reason| format!("accounts entry {}: {reason}", line + 1))
            })
            .collect(),
    })
}

enum Next {
    Done,
    /// A device whose registration has not completed: retried with its saved id.
    Finish(usize),
    Add,
}

fn next_registration(account: &AccountState, wanted: usize) -> Next {
    if let Some(device) = account
        .devices
        .iter()
        .position(|device| !device.retired && device.device_id.is_none())
    {
        return Next::Finish(device);
    }
    if account
        .devices
        .iter()
        .filter(|device| !device.retired)
        .count()
        < wanted
    {
        Next::Add
    } else {
        Next::Done
    }
}

fn store(state: &mut State, account: usize, device: usize, tokens: DeviceTokens) {
    let account = &mut state.accounts[account];
    account.user_id = Some(tokens.user_id);
    let device = &mut account.devices[device];
    device.device_id = Some(tokens.device_id);
    device.refresh_token = Some(tokens.refresh_token);
    device.needs_sign_in = false;
}

fn retire(state: &mut State, account: usize, device: usize) {
    let retired = &mut state.accounts[account].devices[device];
    retired.retired = true;
    retired.refresh_token = None;
    let at = DeviceRef { account, device };
    state
        .pairs
        .retain(|pair| pair.sender != at && pair.receiver != at);
}

async fn prepare_account(
    api: &AuthApi,
    shared: &Shared,
    index: usize,
    account: &Account,
    wanted: usize,
) -> Result<(), Failure> {
    // Devices a run found signed out: sign the same device in again.
    let repairs: Vec<(usize, Uuid)> = shared
        .read(|state| {
            state.accounts[index]
                .devices
                .iter()
                .enumerate()
                .filter(|(_, device)| device.needs_sign_in && !device.retired)
                .filter_map(|(position, device)| device.device_id.map(|id| (position, id)))
                .collect()
        })
        .await;
    for (device, device_id) in repairs {
        match api
            .login_device(&account.email, &account.password, device_id)
            .await
        {
            Ok(tokens) => {
                shared
                    .update(|state| store(state, index, device, tokens))
                    .await?
            }
            Err(ApiError::Rejected { status: 404, .. }) => {
                shared.update(|state| retire(state, index, device)).await?
            }
            Err(error) => return Err(classify(error)),
        }
    }
    let mut registration: Option<String> = None;
    let mut renewed = false;
    loop {
        let next = shared
            .read(|state| next_registration(&state.accounts[index], wanted))
            .await;
        let device = match next {
            Next::Done => return Ok(()),
            Next::Finish(device) => device,
            // Saved before the request: a retry after a crash reuses its registration id.
            Next::Add => {
                shared
                    .update(|state| {
                        let devices = &mut state.accounts[index].devices;
                        devices.push(DeviceState::new());
                        devices.len() - 1
                    })
                    .await?
            }
        };
        let registration_id = shared
            .read(|state| state.accounts[index].devices[device].registration_id)
            .await;
        let token = match &registration {
            Some(token) => token.clone(),
            None => {
                let token = api
                    .login(&account.email, &account.password)
                    .await
                    .map_err(classify)?;
                registration = Some(token.clone());
                token
            }
        };
        let name = format!("push-load {}", device + 1);
        match api.register_device(&token, registration_id, &name).await {
            Ok(tokens) => {
                shared
                    .update(|state| store(state, index, device, tokens))
                    .await?
            }
            // The registration token lasts 5 minutes: sign in again once.
            Err(ApiError::Rejected { status: 401, .. }) if !renewed => {
                registration = None;
                renewed = true;
            }
            // The id of a revoked device stays spent.
            Err(ApiError::Rejected { status: 404, .. }) => {
                shared.update(|state| retire(state, index, device)).await?
            }
            Err(error) => {
                if matches!(error, ApiError::Rejected { status: 409, .. }) {
                    shared.update(|state| retire(state, index, device)).await?;
                }
                return Err(classify(error));
            }
        }
    }
}

async fn prepare_pairs(
    api: &AuthApi,
    target: &Target,
    shared: &Shared,
    indices: &[usize],
    wanted: usize,
) -> anyhow::Result<usize> {
    shared
        .update(|state| {
            let kept: Vec<Pair> = state
                .pairs
                .iter()
                .filter(|pair| state.pair_is_ready(pair))
                .cloned()
                .collect();
            state.pairs = kept;
        })
        .await?;
    let mut ready = shared.read(|state| state.pairs.len()).await;
    let paired: HashSet<usize> = shared
        .read(|state| {
            state
                .pairs
                .iter()
                .flat_map(|pair| [pair.sender.account, pair.receiver.account])
                .collect()
        })
        .await;
    let free: Vec<DeviceRef> = shared
        .read(|state| {
            indices
                .iter()
                .filter(|index| {
                    !paired.contains(index) && state.accounts[**index].failure.is_none()
                })
                .filter_map(|&account| {
                    state.accounts[account]
                        .devices
                        .iter()
                        .position(DeviceState::is_ready)
                        .map(|device| DeviceRef { account, device })
                })
                .collect()
        })
        .await;
    for chunk in free.chunks(2) {
        if ready >= wanted {
            break;
        }
        let [sender, receiver] = chunk else { break };
        let conversation_id = make_pair(api, target, shared, *sender, *receiver).await?;
        shared
            .update(|state| {
                state.pairs.push(Pair {
                    sender: *sender,
                    receiver: *receiver,
                    conversation_id,
                })
            })
            .await?;
        ready += 1;
    }
    Ok(ready)
}

/// Refreshes `device`, saving the rotated token before using it, and connects it,
/// riding out 4006 while the worker relays a new device.
async fn connect_device(
    api: &AuthApi,
    target: &Target,
    shared: &Shared,
    device: DeviceRef,
) -> anyhow::Result<(Socket, Ready)> {
    let refresh_token = shared
        .read(|state| state.device(device).refresh_token.clone())
        .await
        .context("a paired device has no refresh token")?;
    let tokens = match api.refresh(&refresh_token).await {
        Ok(tokens) => tokens,
        Err(error) => {
            // Refused, or failed without an answer (auth-svc may have rotated it): either
            // way the saved token must not be presented again; the next prepare signs the
            // same device in again.
            shared
                .update(|state| state.device_mut(device).needs_sign_in = true)
                .await?;
            bail!("refreshing a paired device failed ({error}); run prepare again to sign it in");
        }
    };
    let access_token = tokens.access_token.clone();
    shared
        .update(|state| state.device_mut(device).refresh_token = Some(tokens.refresh_token))
        .await?;
    let deadline = Instant::now() + READY_WAIT;
    loop {
        match socket::open(target, &access_token).await? {
            Opened::Ready(socket, ready) => return Ok((*socket, ready)),
            Opened::Closed(4006) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_secs(1)).await
            }
            Opened::Closed(code) => {
                bail!("the gateway refused a prepared device with close code {code}")
            }
        }
    }
}

/// The direct conversation of two accounts, made contacts by an invite when they are not yet.
async fn make_pair(
    api: &AuthApi,
    target: &Target,
    shared: &Shared,
    sender: DeviceRef,
    receiver: DeviceRef,
) -> anyhow::Result<Uuid> {
    let (mut a, _) = connect_device(api, target, shared, sender).await?;
    let result = async {
        let (mut b, b_ready) = connect_device(api, target, shared, receiver).await?;
        let conversation = async {
            let open = request::Body::OpenDirect(OpenDirect {
                peer_user_id: id_bytes(b_ready.user),
            });
            if let Ok(response::Body::DirectOpened(opened)) =
                socket::ask(&mut a, 1, open.clone()).await?
            {
                return id(&opened.conversation_id)
                    .map_err(|_| anyhow::anyhow!("an invalid conversation id"));
            }
            let create = request::Body::CreateInvite(CreateInvite {
                expires_in_s: 600,
                max_uses: 1,
            });
            let invite = match socket::ask(&mut a, 2, create).await? {
                Ok(response::Body::Invite(invite)) => invite,
                Ok(_) => bail!("creating an invite was answered with something else"),
                Err(error) => bail!("creating an invite was refused: {:?}", error.code()),
            };
            match socket::ask(
                &mut b,
                1,
                request::Body::RedeemInvite(RedeemInvite { code: invite.code }),
            )
            .await?
            {
                Ok(response::Body::InviteRedeemed(_)) => {}
                Ok(_) => bail!("redeeming the invite was answered with something else"),
                Err(error) => bail!(
                    "redeeming the invite was refused: {:?} (retry after {:?} s)",
                    error.code(),
                    error.retry_after_s
                ),
            }
            match socket::ask(&mut a, 3, open).await? {
                Ok(response::Body::DirectOpened(opened)) => id(&opened.conversation_id)
                    .map_err(|_| anyhow::anyhow!("an invalid conversation id")),
                _ => bail!("the direct conversation could not be opened"),
            }
        }
        .await;
        socket::close(b).await;
        conversation
    }
    .await;
    socket::close(a).await;
    result
}
