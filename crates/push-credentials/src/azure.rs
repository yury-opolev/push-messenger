//! Access tokens from Azure: workload identity (a federated Kubernetes service-account
//! token exchanged at Microsoft Entra ID) or managed identity (IMDS). Plain OAuth 2.0 over
//! rustls; requests time out after 10 s; answers are read up to 64 KiB.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::Deserialize;

use crate::ConfigProblem;
use crate::token::{AccessToken, TokenError, TokenSource};

/// Microsoft Entra ID in the global Azure cloud.
pub const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com/";
/// The Azure Instance Metadata Service's token endpoint.
pub const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ANSWER_BYTES: u64 = 64 * 1024;

/// The identity variables (set by the AKS workload-identity webhook, or by the chart).
/// With `AZURE_FEDERATED_TOKEN_FILE` the source is workload identity, which needs
/// `AZURE_CLIENT_ID` and `AZURE_TENANT_ID`; without it, managed identity, where
/// `AZURE_CLIENT_ID` selects a user-assigned identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureConfig {
    pub client_id: Option<String>,
    pub tenant_id: Option<String>,
    pub federated_token_file: Option<PathBuf>,
    /// `https://`, ending in `/`; plain `http://` only to this machine (tests).
    pub authority_host: String,
}

impl AzureConfig {
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigProblem> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let authority =
            get("AZURE_AUTHORITY_HOST").unwrap_or_else(|| DEFAULT_AUTHORITY_HOST.to_owned());
        let authority_host = check_authority(&authority)
            .map_err(|reason| ConfigProblem::Invalid {
                name: "AZURE_AUTHORITY_HOST",
                reason,
            })?
            .to_string();
        let federated_token_file = get("AZURE_FEDERATED_TOKEN_FILE").map(PathBuf::from);
        let client_id = get("AZURE_CLIENT_ID");
        let tenant_id = get("AZURE_TENANT_ID");
        if federated_token_file.is_some() {
            if client_id.is_none() {
                return Err(ConfigProblem::Missing("AZURE_CLIENT_ID"));
            }
            match &tenant_id {
                None => return Err(ConfigProblem::Missing("AZURE_TENANT_ID")),
                Some(tenant) if !is_tenant(tenant) => {
                    return Err(ConfigProblem::Invalid {
                        name: "AZURE_TENANT_ID",
                        reason: "expected a tenant id or domain".to_owned(),
                    });
                }
                Some(_) => {}
            }
        }
        Ok(Self {
            client_id,
            tenant_id,
            federated_token_file,
            authority_host,
        })
    }

    /// The token source these variables describe.
    pub fn source(&self) -> Result<Arc<dyn TokenSource>, TokenError> {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| TokenError::Config("cannot build the HTTP client".to_owned()))?;
        match &self.federated_token_file {
            Some(file) => Ok(Arc::new(WorkloadIdentity::new(
                http,
                &self.authority_host,
                self.tenant_id.as_deref().unwrap_or_default(),
                self.client_id.as_deref().unwrap_or_default(),
                file,
            )?)),
            None => Ok(Arc::new(ManagedIdentity::new(
                http,
                IMDS_ENDPOINT,
                self.client_id.clone(),
            )?)),
        }
    }
}

fn check_authority(value: &str) -> Result<Url, String> {
    let mut url = Url::parse(value).map_err(|_| "not a URL".to_owned())?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("must be an https:// URL".to_owned()),
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("must be a plain base URL".to_owned());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn is_tenant(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// Workload identity: the projected service-account token at `token_file` (re-read on
/// every request; the kubelet rotates it) is exchanged for an access token.
pub struct WorkloadIdentity {
    http: reqwest::Client,
    token_url: Url,
    client_id: String,
    token_file: PathBuf,
}

impl WorkloadIdentity {
    pub fn new(
        http: reqwest::Client,
        authority_host: &str,
        tenant_id: &str,
        client_id: &str,
        token_file: &Path,
    ) -> Result<Self, TokenError> {
        let authority = check_authority(authority_host).map_err(TokenError::Config)?;
        if !is_tenant(tenant_id) || client_id.is_empty() {
            return Err(TokenError::Config(
                "a client id and a tenant id are required".to_owned(),
            ));
        }
        let token_url = authority
            .join(&format!("{tenant_id}/oauth2/v2.0/token"))
            .map_err(|_| TokenError::Config("cannot build the token URL".to_owned()))?;
        Ok(Self {
            http,
            token_url,
            client_id: client_id.to_owned(),
            token_file: token_file.to_owned(),
        })
    }
}

#[async_trait]
impl TokenSource for WorkloadIdentity {
    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError> {
        let assertion = tokio::fs::read_to_string(&self.token_file)
            .await
            .map_err(|error| TokenError::File(error.kind().to_string()))?;
        let form = [
            ("client_id", self.client_id.as_str()),
            ("scope", scope),
            ("grant_type", "client_credentials"),
            ("client_assertion_type", JWT_BEARER),
            ("client_assertion", assertion.trim()),
        ];
        answer(
            self.http
                .post(self.token_url.clone())
                .form(&form)
                .send()
                .await,
        )
        .await
    }
}

/// Managed identity through IMDS; `client_id` selects a user-assigned identity.
pub struct ManagedIdentity {
    http: reqwest::Client,
    endpoint: Url,
    client_id: Option<String>,
}

impl ManagedIdentity {
    pub fn new(
        http: reqwest::Client,
        endpoint: &str,
        client_id: Option<String>,
    ) -> Result<Self, TokenError> {
        let endpoint = Url::parse(endpoint).map_err(|_| {
            TokenError::Config("the managed-identity endpoint is not a URL".to_owned())
        })?;
        Ok(Self {
            http,
            endpoint,
            client_id,
        })
    }
}

#[async_trait]
impl TokenSource for ManagedIdentity {
    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError> {
        let resource = scope.strip_suffix("/.default").unwrap_or(scope);
        let mut query = vec![("api-version", "2018-02-01"), ("resource", resource)];
        if let Some(client_id) = &self.client_id {
            query.push(("client_id", client_id));
        }
        let request = self
            .http
            .get(self.endpoint.clone())
            .query(&query)
            .header("Metadata", "true");
        answer(request.send().await).await
    }
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: Lifetime,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Lifetime {
    Seconds(u64),
    Text(String),
}

#[derive(Deserialize)]
struct ErrorAnswer {
    error: Option<String>,
}

async fn answer(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<AccessToken, TokenError> {
    let response = response.map_err(|error| unreachable(&error))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ANSWER_BYTES)
    {
        return Err(TokenError::Malformed);
    }
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| unreachable(&error))?;
    if body.len() as u64 > MAX_ANSWER_BYTES {
        return Err(TokenError::Malformed);
    }
    if !status.is_success() {
        // Only a short machine code passes; the description may echo request data.
        let code = serde_json::from_slice::<ErrorAnswer>(&body)
            .ok()
            .and_then(|answer| answer.error)
            .filter(|code| {
                code.len() <= 64 && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            });
        return Err(TokenError::Refused {
            status: status.as_u16(),
            code,
        });
    }
    let answer: TokenAnswer = serde_json::from_slice(&body).map_err(|_| TokenError::Malformed)?;
    let seconds = match answer.expires_in {
        Lifetime::Seconds(seconds) => seconds,
        Lifetime::Text(text) => text.parse().map_err(|_| TokenError::Malformed)?,
    };
    if answer.access_token.is_empty() || seconds == 0 {
        return Err(TokenError::Malformed);
    }
    Ok(AccessToken::new(
        answer.access_token,
        Duration::from_secs(seconds),
    ))
}

fn unreachable(error: &reqwest::Error) -> TokenError {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    TokenError::Unreachable(kind.to_owned())
}
