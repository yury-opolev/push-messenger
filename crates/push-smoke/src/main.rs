use std::process::ExitCode;
use std::time::Duration;

use push_smoke::{Target, run};

/// The whole run, so that a hung deployment fails a helm test instead of hanging it.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(180);

#[tokio::main]
async fn main() -> ExitCode {
    let target = match Target::parse(std::env::args().skip(1), |name| std::env::var(name).ok()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match tokio::time::timeout(TOTAL_TIMEOUT, run(&target)).await {
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
