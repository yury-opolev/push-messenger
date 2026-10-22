use std::process::ExitCode;
use std::time::Duration;

use push_smoke::{Target, run};

/// The whole run, so that a hung deployment fails a helm test instead of hanging it.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // DEV ONLY: with the rehearsal issuer's token endpoint the Entra ID smoke test runs.
    let entra_token_url = match take_flag(&mut args, "--entra-token-url") {
        Ok(url) => url
            .or_else(|| std::env::var("PUSH_SMOKE_ENTRA_TOKEN_URL").ok())
            .filter(|url| !url.is_empty()),
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let entra_accounts = match take_flag(&mut args, "--entra-accounts") {
        Ok(path) => path
            .or_else(|| std::env::var("PUSH_SMOKE_ENTRA_ACCOUNTS").ok())
            .filter(|path| !path.is_empty()),
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    if entra_accounts.is_some() && entra_token_url.is_some() {
        eprintln!(
            "--entra-accounts and --entra-token-url (including environment settings) are mutually exclusive"
        );
        return ExitCode::from(2);
    }
    let accounts = match entra_accounts {
        Some(path) => match push_load::accounts::read(std::path::Path::new(&path)) {
            Ok(accounts) => Some(accounts),
            Err(error) => {
                eprintln!("smoke: invalid accounts: {error}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let parse = if accounts.is_some() || entra_token_url.is_some() {
        Target::parse_entra
    } else {
        Target::parse
    };
    let target = match parse(args, |name: &str| std::env::var(name).ok()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let smoke = async {
        match (&accounts, &entra_token_url) {
            (Some(accounts), None) => push_smoke::entra::run_accounts(&target, accounts).await,
            (None, Some(url)) => push_smoke::entra::run(&target, url).await,
            _ => run(&target).await,
        }
    };
    match tokio::time::timeout(TOTAL_TIMEOUT, smoke).await {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            eprintln!("smoke: FAIL: {error:#}");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("smoke: FAIL: not finished within {TOTAL_TIMEOUT:?}");
            ExitCode::FAILURE
        }
    }
}

/// Removes `flag` and its value from `args`, returning the value.
fn take_flag(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return Ok(None);
    };
    args.remove(index);
    if args.iter().any(|arg| arg == flag) {
        return Err(format!("{flag} may only appear once"));
    }
    if index < args.len() && !args[index].starts_with("--") && !args[index].trim().is_empty() {
        Ok(Some(args.remove(index)))
    } else {
        Err(format!("{flag} needs a value"))
    }
}
