//! DEV ONLY: creates verified accounts for push-load on a local deployment whose worker
//! runs the log sender with the DEV ONLY mailbox (minikube). Never for a real deployment:
//! there, accounts come from the consumer's normal sign-up and verification.
//!
//!   push-dev-accounts --auth <url> --mailbox <url> --count <1-20> --out <file.jsonl> [--host <host>]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};

const USAGE: &str = "usage: push-dev-accounts --auth <url> --mailbox <url> --count <1-20> --out <file.jsonl> [--host <host>]";

#[tokio::main]
async fn main() -> ExitCode {
    match create(std::env::args().skip(1).collect()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("push-dev-accounts: FAIL: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn create(args: Vec<String>) -> anyhow::Result<()> {
    let (mut auth, mut mailbox, mut count, mut out, mut host) = (None, None, None, None, None);
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--auth" => &mut auth,
            "--mailbox" => &mut mailbox,
            "--count" => &mut count,
            "--out" => &mut out,
            "--host" => &mut host,
            other => bail!("unknown argument {other:?}; {USAGE}"),
        };
        *slot = Some(
            args.next()
                .with_context(|| format!("{flag} needs a value"))?,
        );
    }
    let count: usize = count
        .context(USAGE)?
        .parse()
        .context("--count must be a number")?;
    if !(1..=20).contains(&count) {
        bail!("--count must be 1 to 20: auth-svc allows 20 signups per IP an hour");
    }
    let out = PathBuf::from(out.context(USAGE)?);
    push_load::state::ensure_outside_git(&out)?;
    if out.exists() {
        bail!("{} exists: name a new file", out.display());
    }
    let target = push_smoke::Target {
        auth_url: auth.context(USAGE)?,
        mailbox_url: mailbox.context(USAGE)?,
        socket_url: String::new(),
        host,
        signing_key: String::new(),
        data_dir: String::new(),
    };
    let accounts = push_smoke::create_verified_accounts(&target, count).await?;
    push_load::state::write_private(&out, push_load::accounts::to_lines(&accounts)?.as_bytes())?;
    println!(
        "push-dev-accounts: wrote {} DEV ONLY verified accounts to {}",
        accounts.len(),
        out.display()
    );
    Ok(())
}
