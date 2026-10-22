//! Operator mode must work without DEV credentials and must never contact a minter.
use std::sync::Arc;

use axum::{Json, Router, extract::Request, routing::post};
use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[test]
fn operator_cli_rejects_conflicting_modes_including_environment() {
    for in_environment in [false, true] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_push-smoke"));
        command.args(["--entra-accounts", "unused.jsonl"]);
        command.env_remove("PUSH_SMOKE_ENTRA_TOKEN_URL");
        if in_environment {
            command.env("PUSH_SMOKE_ENTRA_TOKEN_URL", "http://stub/dev/token");
        } else {
            command.args(["--entra-token-url", "http://stub/dev/token"]);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("mutually exclusive"));
    }
}

#[tokio::test]
async fn operator_cli_uses_entra_for_inline_and_file_tokens_and_redacts_http_errors() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join(format!(".push-smoke-operator-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = |oid| {
        format!(
            "e30.{}.secret-signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                json!({
                    "tid": uuid::Uuid::nil(), "oid": oid,
                })
                .to_string()
            )
        )
    };
    let first = token("11111111-1111-1111-1111-111111111111");
    let second = token("22222222-2222-2222-2222-222222222222");
    let seen = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let requests = seen.clone();
    let echo = first.clone();
    let router = Router::new().fallback(post(move |request: Request| {
        let seen = seen.clone();
        let echo = echo.clone();
        async move {
            let path = request.uri().path().to_owned();
            let body = serde_json::from_slice(
                &axum::body::to_bytes(request.into_body(), 8192)
                    .await
                    .unwrap(),
            )
            .unwrap();
            seen.lock().await.push((path, body));
            (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error":{"code":echo,"message":echo}})),
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(axum::serve(listener, router).into_future());
    for file in [false, true] {
        let entry = if file {
            std::fs::write(dir.join("first.jwt"), &first).unwrap();
            json!({"entra_token_file":"first.jwt"})
        } else {
            json!({"entra_token":first})
        };
        let path = dir.join("accounts.jsonl");
        std::fs::write(&path, format!("{entry}\n{}", json!({"entra_token":second}))).unwrap();
        let data = dir.to_string_lossy().into_owned();
        let url = url.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_push-smoke"))
                .args([
                    "--auth",
                    &url,
                    "--socket",
                    "ws://127.0.0.1:1/v1/socket",
                    "--data-dir",
                    &data,
                    "--entra-accounts",
                ])
                .arg(path)
                .env_remove("PUSH_SMOKE_SIGNING_KEY")
                .env_remove("PUSH_SMOKE_MAILBOX_URL")
                .env_remove("PUSH_SMOKE_ENTRA_TOKEN_URL")
                .env_remove("PUSH_SMOKE_ENTRA_ACCOUNTS")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(!output.status.success());
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.contains(&first) && !output.contains(&second),
            "HTTP diagnostics must be redacted"
        );
    }
    let requests = requests.lock().await;
    assert_eq!(
        requests.len(),
        2,
        "both operator runs must reach auth without DEV configuration"
    );
    for (path, body) in requests.iter() {
        assert_eq!(
            path, "/v1/auth/entra",
            "operator mode must not call the stub or password routes"
        );
        assert_eq!(body["token"], first);
        assert!(
            body["handle"].is_null(),
            "operator accounts must already exist"
        );
    }
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
