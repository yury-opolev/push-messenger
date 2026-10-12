use std::process::ExitCode;

use push_smoke::{Target, run};

#[tokio::main]
async fn main() -> ExitCode {
    let target = match Target::parse(std::env::args().skip(1), |name| std::env::var(name).ok()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match run(&target).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("smoke: FAIL: {error:#}");
            ExitCode::FAILURE
        }
    }
}
