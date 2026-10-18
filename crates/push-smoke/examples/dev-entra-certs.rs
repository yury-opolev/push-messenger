//! DEV ONLY: a throwaway test CA and one server certificate for the Entra ID rehearsal on
//! a local cluster (Plan 12B, `deploy/minikube/entra-rehearsal.ps1`). Never for real
//! servers.
//!
//!   cargo run -p push-smoke --example dev-entra-certs -- <dir> <dns name>...
//!
//! Writes `ca.crt`, `server.crt` and `server.key` to `<dir>`. An example, not a binary:
//! the test CA (rcgen) needs newer Rust than the MSRV the binaries are built with.

use std::path::Path;

use push_credentials::testing::TestCa;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, names @ ..] = &args[..] else {
        anyhow::bail!("usage: dev-entra-certs <dir> <dns name>...");
    };
    if names.is_empty() {
        anyhow::bail!("usage: dev-entra-certs <dir> <dns name>...");
    }
    let dir = Path::new(dir);
    std::fs::create_dir_all(dir)?;
    let ca = TestCa::new();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (cert, key) = ca.server_for(&names);
    std::fs::write(dir.join("ca.crt"), ca.pem())?;
    std::fs::write(dir.join("server.crt"), cert)?;
    std::fs::write(dir.join("server.key"), key)?;
    println!(
        "wrote ca.crt, server.crt and server.key (DEV ONLY) to {}",
        dir.display()
    );
    Ok(())
}
