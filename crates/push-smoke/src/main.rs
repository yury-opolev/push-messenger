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
    let target = match Target::parse(args, |name| std::env::var(name).ok()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let smoke = async {
        match &entra_token_url {
            Some(url) => push_smoke::entra::run(&target, url).await,
            None => run(&target).await,
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
    if index < args.len() {
        Ok(Some(args.remove(index)))
    } else {
        Err(format!("{flag} needs a value"))
    }
}
