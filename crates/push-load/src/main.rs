use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match push_load::cli::main(std::env::args().skip(1).collect()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("push-load: {error:#}");
            ExitCode::from(2)
        }
    }
}
