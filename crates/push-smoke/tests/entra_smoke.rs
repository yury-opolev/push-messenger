//! The DEV ONLY Entra mode of the smoke test (Plan 13B) against the whole stack with
//! Microsoft Entra ID sign-in only: auth-svc trusts the stub issuer, the worker sends no
//! email, and a local token endpoint mints user tokens as `push-dev-entra sign-in` does in
//! the minikube rehearsal.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use common::Stack;
use push_auth_core::entra::testing::StubOpenId;
use serde_json::{Value, json};

#[tokio::test]
async fn the_entra_smoke_test_passes_against_an_entra_only_stack() {
    let stub = Arc::new(StubOpenId::start().await);
    let (mut datastores, _containers) = common::password_datastores().await;
    datastores.auth.extend(stub.variables());
    datastores.worker.extend([
        ("PUSH_EMAIL_SENDER".to_owned(), "none".to_owned()),
        ("PUSH_DEV_MAILBOX".to_owned(), "0".to_owned()),
    ]);
    let stack = Stack::start(datastores).await;

    let minter = Arc::clone(&stub);
    let tokens = Router::new().route(
        "/dev/token",
        post(move |Json(body): Json<Value>| {
            let minter = Arc::clone(&minter);
            async move { Json(json!({ "token": minter.token(body["oid"].as_str().unwrap()) })) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/dev/token", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, tokens).into_future());

    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-smoke-entra-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let target = stack.smoke_target(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(
        Duration::from_secs(300),
        push_smoke::entra::run(&target, &token_url),
    )
    .await
    .expect("the Entra smoke test finishes within 300 s")
    .expect("the Entra smoke test passes");
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&data_dir);
}
