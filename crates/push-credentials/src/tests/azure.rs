//! The Azure token sources against the stub issuer.

use std::time::Duration;

use crate::testing::{StubIssuer, StubPrincipal};
use crate::{
    AzureConfig, ConfigProblem, ManagedIdentity, TokenError, TokenSource, WorkloadIdentity,
};

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";

fn principal() -> Vec<StubPrincipal> {
    vec![StubPrincipal {
        client_id: "client-a".to_owned(),
        object_id: "oid-a".to_owned(),
    }]
}

fn assertion_file(contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("push-credentials-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&path, format!("{contents}\n")).unwrap();
    path
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn stub() -> StubIssuer {
    StubIssuer::start(
        TENANT,
        "federated",
        principal(),
        Duration::from_secs(3600),
        None,
    )
    .await
}

#[tokio::test]
async fn workload_identity_exchanges_the_federated_token_for_an_access_token() {
    let stub = stub().await;
    let source = WorkloadIdentity::new(
        http(),
        stub.authority(),
        TENANT,
        "client-a",
        &assertion_file("federated"),
    )
    .unwrap();
    let token = source.fetch(SCOPE).await.unwrap();
    assert_eq!(token.object_id().as_deref(), Some("oid-a"));
    let issued = stub.issued();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].scope, SCOPE);
    assert_eq!(issued[0].token, token.secret());
}

#[tokio::test]
async fn a_refusal_reports_the_status_and_code_but_never_the_description() {
    let stub = stub().await;
    let source = WorkloadIdentity::new(
        http(),
        stub.authority(),
        TENANT,
        "client-a",
        &assertion_file("wrong-assertion"),
    )
    .unwrap();
    let error = source.fetch(SCOPE).await.unwrap_err();
    assert_eq!(
        error,
        TokenError::Refused {
            status: 401,
            code: Some("invalid_client".to_owned())
        }
    );
    let text = error.to_string();
    assert!(
        !text.contains("AADSTS") && !text.contains("leaked") && !text.contains("wrong-assertion"),
        "{text}"
    );
}

#[tokio::test]
async fn managed_identity_asks_imds_with_the_resource_and_reads_a_text_lifetime() {
    let stub = stub().await;
    let source =
        ManagedIdentity::new(http(), &stub.imds_endpoint(), Some("client-a".to_owned())).unwrap();
    let token = source.fetch(SCOPE).await.unwrap();
    assert_eq!(token.object_id().as_deref(), Some("oid-a"));
    assert_eq!(stub.issued()[0].scope, SCOPE);
}

#[tokio::test]
async fn an_unreachable_endpoint_and_a_missing_file_are_reported_without_details() {
    let closed = WorkloadIdentity::new(
        http(),
        "http://127.0.0.1:9/",
        TENANT,
        "client-a",
        &assertion_file("x"),
    )
    .unwrap();
    assert!(matches!(
        closed.fetch(SCOPE).await,
        Err(TokenError::Unreachable(_))
    ));
    let stub = stub().await;
    let missing = WorkloadIdentity::new(
        http(),
        stub.authority(),
        TENANT,
        "client-a",
        &std::env::temp_dir().join("push-credentials-no-such-file.jwt"),
    )
    .unwrap();
    assert!(matches!(
        missing.fetch(SCOPE).await,
        Err(TokenError::File(_))
    ));
}

fn lookup(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    }
}

#[test]
fn the_configuration_requires_https_except_on_this_machine() {
    let workload = vec![
        ("AZURE_CLIENT_ID", "client-a"),
        ("AZURE_TENANT_ID", TENANT),
        (
            "AZURE_FEDERATED_TOKEN_FILE",
            "/var/run/secrets/azure/tokens/azure-identity-token",
        ),
    ];
    let config = AzureConfig::from_lookup(&lookup(workload)).unwrap();
    assert_eq!(config.authority_host, "https://login.microsoftonline.com/");
    assert_eq!(config.client_id.as_deref(), Some("client-a"));
    let local = AzureConfig::from_lookup(&lookup(vec![(
        "AZURE_AUTHORITY_HOST",
        "http://127.0.0.1:8080",
    )]))
    .unwrap();
    assert_eq!(local.authority_host, "http://127.0.0.1:8080/");
    assert!(local.federated_token_file.is_none(), "managed identity");
    for bad in [
        "http://login.example.com/",
        "ftp://x/",
        "https://x/?q=1",
        "not a url",
    ] {
        assert!(
            matches!(
                AzureConfig::from_lookup(&lookup(vec![("AZURE_AUTHORITY_HOST", bad)])),
                Err(ConfigProblem::Invalid {
                    name: "AZURE_AUTHORITY_HOST",
                    ..
                })
            ),
            "{bad}"
        );
    }
    assert_eq!(
        AzureConfig::from_lookup(&lookup(vec![("AZURE_FEDERATED_TOKEN_FILE", "/t")])),
        Err(ConfigProblem::Missing("AZURE_CLIENT_ID"))
    );
    assert_eq!(
        AzureConfig::from_lookup(&lookup(vec![
            ("AZURE_FEDERATED_TOKEN_FILE", "/t"),
            ("AZURE_CLIENT_ID", "c")
        ])),
        Err(ConfigProblem::Missing("AZURE_TENANT_ID"))
    );
}
