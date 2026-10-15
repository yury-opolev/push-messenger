//! A fake auth-svc and a gateway that answers by itself (`push_sdk::testing`), for the
//! bindings' tests run outside Rust (Node's): prints their URLs and the identity they
//! issue as one JSON line, then serves until its standard input closes.

use std::io::Write;

use push_sdk::testing::{AutoGateway, FakeAuth};

#[tokio::main]
async fn main() {
    let auth = FakeAuth::start().await;
    let gateway = AutoGateway::start(auth.identity.clone()).await;
    let line = serde_json::json!({
        "authUrl": auth.url,
        "socketUrl": gateway.url,
        "userId": auth.user.to_string(),
        "deviceId": auth.device.to_string(),
    });
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
    let _ =
        tokio::task::spawn_blocking(|| std::io::copy(&mut std::io::stdin(), &mut std::io::sink()))
            .await;
}
