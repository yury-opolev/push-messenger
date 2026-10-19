//! Which ways to sign in auth-svc offers (`PUSH_AUTH_PROVIDERS`) and how it checks
//! Microsoft Entra ID access tokens (`PUSH_ENTRA_*`, spec 2026-10-19 §3).

use std::time::Duration;

use reqwest::Url;
use uuid::Uuid;

use crate::config::ConfigError;

/// Microsoft Entra ID in the global Azure cloud.
pub const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com/";
/// The delegated scope required by default: the one the guide tells the API registration
/// to expose.
pub const DEFAULT_REQUIRED_SCOPE: &str = "access_as_user";
/// The tenant of personal Microsoft accounts.
pub const CONSUMERS_TENANT: Uuid = Uuid::from_u128(0x9188040d_6c67_4c5b_b112_36a304b66dad);
const MAX_TENANTS: usize = 20;
const MAX_AUDIENCES: usize = 4;
const MAX_SESSION_HOURS: u64 = 720;

/// The ways to sign in: email and password, Microsoft Entra ID, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Providers {
    pub password: bool,
    pub entra: bool,
}

impl Default for Providers {
    /// Email and password only: today's behaviour.
    fn default() -> Self {
        Self {
            password: true,
            entra: false,
        }
    }
}

impl Providers {
    /// A `PUSH_AUTH_PROVIDERS` value: `password`, `entra` or both, comma-separated, each
    /// once; unset or empty is `password`.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::default());
        };
        let mut providers = Self {
            password: false,
            entra: false,
        };
        for name in value.split(',').map(str::trim) {
            match name {
                "password" if !providers.password => providers.password = true,
                "entra" if !providers.entra => providers.entra = true,
                _ => return Err("expected password, entra or password,entra".to_owned()),
            }
        }
        Ok(providers)
    }
}

/// How Entra ID access tokens are checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraSettings {
    /// Scheme and host, ending in `/`: `https://login.microsoftonline.com/`.
    pub authority_host: String,
    /// Whose users may sign in.
    pub tenants: Tenants,
    /// The accepted `aud` values: the API's application id (what v2.0 tokens carry) and
    /// optionally its `api://` identifier URI.
    pub audiences: Vec<String>,
    /// The delegated scope `scp` must contain; `None` accepts any scope of this API.
    pub required_scope: Option<String>,
    /// The app role `roles` must contain, if any.
    pub required_role: Option<String>,
    /// The client apps (`azp`) allowed to present tokens; empty allows any.
    pub client_apps: Vec<Uuid>,
    /// How long an Entra-backed device stays signed in without a new Entra sign-in.
    pub max_session: Duration,
}

impl EntraSettings {
    /// Reads the `PUSH_ENTRA_*` variables. Their values are configuration, not secrets, so
    /// errors show them.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let invalid = |name: &'static str, value: &str, reason: &str| ConfigError::Invalid {
            name,
            value: value.to_owned(),
            reason: reason.to_owned(),
        };

        let authority =
            get("PUSH_ENTRA_AUTHORITY_HOST").unwrap_or_else(|| DEFAULT_AUTHORITY_HOST.to_owned());
        let authority_host = authority_host(&authority)
            .map_err(|reason| invalid("PUSH_ENTRA_AUTHORITY_HOST", &authority, reason))?;

        let tenants_value = get("PUSH_ENTRA_ALLOWED_TENANTS")
            .ok_or(ConfigError::Missing("PUSH_ENTRA_ALLOWED_TENANTS"))?;
        let tenants = Tenants::parse(&tenants_value)
            .map_err(|reason| invalid("PUSH_ENTRA_ALLOWED_TENANTS", &tenants_value, reason))?;

        let audiences_value =
            get("PUSH_ENTRA_CLIENT_ID").ok_or(ConfigError::Missing("PUSH_ENTRA_CLIENT_ID"))?;
        let audiences = audiences(&audiences_value)
            .map_err(|reason| invalid("PUSH_ENTRA_CLIENT_ID", &audiences_value, reason))?;

        let scope_value =
            get("PUSH_ENTRA_REQUIRED_SCOPE").unwrap_or_else(|| DEFAULT_REQUIRED_SCOPE.to_owned());
        let required_scope = match scope_value.as_str() {
            "*" => None,
            scope if is_claim_value(scope) => Some(scope.to_owned()),
            _ => {
                return Err(invalid(
                    "PUSH_ENTRA_REQUIRED_SCOPE",
                    &scope_value,
                    "use one scope name (1 to 128 visible ASCII characters) or *",
                ));
            }
        };
        let required_role = match get("PUSH_ENTRA_REQUIRED_ROLE") {
            None => None,
            Some(role) if is_claim_value(&role) => Some(role),
            Some(role) => {
                return Err(invalid(
                    "PUSH_ENTRA_REQUIRED_ROLE",
                    &role,
                    "use one app role value (1 to 128 visible ASCII characters)",
                ));
            }
        };
        let client_apps = match get("PUSH_ENTRA_ALLOWED_CLIENT_APPS") {
            None => Vec::new(),
            Some(value) => guids(&value, MAX_TENANTS)
                .map_err(|reason| invalid("PUSH_ENTRA_ALLOWED_CLIENT_APPS", &value, reason))?,
        };
        let hours_value = get("PUSH_ENTRA_MAX_SESSION_HOURS").unwrap_or_else(|| "24".to_owned());
        let hours: u64 = hours_value
            .parse()
            .ok()
            .filter(|hours| (1..=MAX_SESSION_HOURS).contains(hours))
            .ok_or_else(|| {
                invalid(
                    "PUSH_ENTRA_MAX_SESSION_HOURS",
                    &hours_value,
                    "use a whole number of hours from 1 to 720",
                )
            })?;
        Ok(Self {
            authority_host,
            tenants,
            audiences,
            required_scope,
            required_role,
            client_apps,
            max_session: Duration::from_secs(hours * 3600),
        })
    }

    /// The `iss` of `tenant`'s v2.0 tokens.
    pub fn issuer(&self, tenant: Uuid) -> String {
        format!("{}{tenant}/v2.0", self.authority_host)
    }

    /// The OpenID Connect metadata document of a key source ([`Tenants::key_source`]).
    pub fn metadata_url(&self, source: &str) -> String {
        format!(
            "{}{source}/v2.0/.well-known/openid-configuration",
            self.authority_host
        )
    }
}

/// `https://host/` (a sovereign cloud's too), or `http://` to a loopback host for tests.
fn authority_host(value: &str) -> Result<String, &'static str> {
    let url = Url::parse(value).map_err(|_| "not a URL")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("must be an https:// URL (http:// only for a loopback host)"),
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("must be a scheme and host only, like https://login.microsoftonline.com/");
    }
    Ok(url.to_string())
}

/// Whose users may sign in: `PUSH_ENTRA_ALLOWED_TENANTS` (spec D3). Exactly one keyword,
/// or one or more tenant ids; never a mixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tenants {
    /// Any tenant, personal Microsoft accounts included.
    Common,
    /// Any work or school tenant: every tenant but the personal-account one.
    Organizations,
    /// Personal Microsoft accounts only: [`CONSUMERS_TENANT`].
    Consumers,
    /// Exactly these tenants.
    Ids(Vec<Uuid>),
}

impl Tenants {
    /// `common`, `organizations` or `consumers` alone (any case), or tenant ids separated by
    /// commas: no keyword among them, no empty entry, no duplicate, not the nil id, at most
    /// [`MAX_TENANTS`].
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let value = value.trim();
        match value.to_ascii_lowercase().as_str() {
            "common" => return Ok(Self::Common),
            "organizations" => return Ok(Self::Organizations),
            "consumers" => return Ok(Self::Consumers),
            _ => {}
        }
        let mut ids: Vec<Uuid> = Vec::new();
        for item in value.split(',').map(str::trim) {
            if item.is_empty() {
                return Err("an empty entry: list tenant ids separated by commas");
            }
            if matches!(
                item.to_ascii_lowercase().as_str(),
                "common" | "organizations" | "consumers"
            ) {
                return Err("common, organizations and consumers stand alone: never combined");
            }
            let id = Uuid::try_parse(item).map_err(|_| {
                "expected common, organizations, consumers, or tenant ids (GUIDs) separated by commas"
            })?;
            if id.is_nil() {
                return Err("the nil GUID is not a tenant");
            }
            if ids.contains(&id) {
                return Err("a tenant id is listed twice");
            }
            ids.push(id);
        }
        if ids.len() > MAX_TENANTS {
            return Err("at most 20 tenant ids");
        }
        Ok(Self::Ids(ids))
    }

    /// Whether a token of tenant `tid` may sign in.
    pub fn admits(&self, tid: Uuid) -> bool {
        match self {
            Self::Common => !tid.is_nil(),
            Self::Organizations => !tid.is_nil() && tid != CONSUMERS_TENANT,
            Self::Consumers => tid == CONSUMERS_TENANT,
            Self::Ids(ids) => ids.contains(&tid),
        }
    }

    /// The authority path whose metadata and keys sign `tid`'s tokens: the tenant itself in
    /// a list, the keyword's shared endpoint otherwise (spec D4); `None` if not admitted.
    pub fn key_source(&self, tid: Uuid) -> Option<String> {
        if !self.admits(tid) {
            return None;
        }
        Some(match self {
            Self::Ids(_) => tid.to_string(),
            keyword => keyword.key_sources().remove(0),
        })
    }

    /// Every key source: one per listed tenant, or the keyword itself. This bounds the key
    /// cache whatever `tid`s tokens carry.
    pub fn key_sources(&self) -> Vec<String> {
        match self {
            Self::Common => vec!["common".to_owned()],
            Self::Organizations => vec!["organizations".to_owned()],
            Self::Consumers => vec!["consumers".to_owned()],
            Self::Ids(ids) => ids.iter().map(Uuid::to_string).collect(),
        }
    }
}

/// 1 to `most` GUIDs, comma-separated; duplicates count once.
fn guids(value: &str, most: usize) -> Result<Vec<Uuid>, &'static str> {
    let mut ids = Vec::new();
    for item in value.split(',').map(str::trim) {
        let id = Uuid::try_parse(item).map_err(|_| "expected ids (GUIDs) separated by commas")?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.len() > most {
        return Err("too many ids");
    }
    Ok(ids)
}

/// The API's application id, optionally with its `api://` identifier URI.
fn audiences(value: &str) -> Result<Vec<String>, &'static str> {
    let mut audiences = Vec::new();
    for item in value.split(',').map(str::trim) {
        let audience = if let Ok(id) = Uuid::try_parse(item) {
            id.to_string()
        } else if item.len() > "api://".len()
            && item.len() <= 256
            && item.starts_with("api://")
            && item.bytes().all(|byte| byte.is_ascii_graphic())
        {
            item.to_owned()
        } else {
            return Err(
                "expected the API's application id, optionally with its api:// identifier URI",
            );
        };
        if !audiences.contains(&audience) {
            audiences.push(audience);
        }
    }
    if audiences.len() > MAX_AUDIENCES {
        return Err("at most four audiences");
    }
    if audiences
        .iter()
        .all(|audience| audience.starts_with("api://"))
    {
        return Err("include the API's application id: v2.0 access tokens carry it as `aud`");
    }
    Ok(audiences)
}

/// A scope or role value: 1 to 128 visible ASCII characters.
fn is_claim_value(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const TENANT: &str = "33333333-0000-4000-8000-000000000003";
    const API: &str = "11111111-0000-4000-8000-000000000001";

    fn settings(variables: &[(&str, &str)]) -> Result<EntraSettings, ConfigError> {
        let mut all: HashMap<String, String> = [
            ("PUSH_ENTRA_ALLOWED_TENANTS", TENANT),
            ("PUSH_ENTRA_CLIENT_ID", API),
        ]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        for (name, value) in variables {
            all.insert(name.to_string(), value.to_string());
        }
        EntraSettings::from_lookup(&|name: &str| all.get(name).cloned())
    }

    fn refused(name: &str, value: &str) -> bool {
        matches!(
            settings(&[(name, value)]),
            Err(ConfigError::Invalid { name: refused, .. }) if refused == name
        )
    }

    #[test]
    fn providers_default_to_password_and_take_each_name_once() {
        assert_eq!(
            Providers::parse(None),
            Ok(Providers {
                password: true,
                entra: false
            })
        );
        assert_eq!(Providers::parse(Some(" ")), Ok(Providers::default()));
        assert_eq!(
            Providers::parse(Some("entra")),
            Ok(Providers {
                password: false,
                entra: true
            })
        );
        assert_eq!(
            Providers::parse(Some("entra, password")),
            Ok(Providers {
                password: true,
                entra: true
            })
        );
        for bad in ["Entra", "oidc", "password,password", "password,", ","] {
            assert!(Providers::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_defaults_name_the_global_cloud_a_day_and_access_as_user() {
        let settings = settings(&[]).unwrap();
        assert_eq!(settings.authority_host, DEFAULT_AUTHORITY_HOST);
        let tenant = Uuid::parse_str(TENANT).unwrap();
        assert_eq!(settings.tenants, Tenants::Ids(vec![tenant]));
        assert_eq!(settings.audiences, [API]);
        assert_eq!(settings.required_scope.as_deref(), Some("access_as_user"));
        assert_eq!(settings.required_role, None);
        assert!(settings.client_apps.is_empty());
        assert_eq!(settings.max_session, Duration::from_secs(24 * 3600));
        assert_eq!(
            settings.issuer(tenant),
            format!("https://login.microsoftonline.com/{TENANT}/v2.0")
        );
        assert_eq!(
            settings.metadata_url(TENANT),
            format!(
                "https://login.microsoftonline.com/{TENANT}/v2.0/.well-known/openid-configuration"
            )
        );
    }

    #[test]
    fn tenants_are_one_keyword_or_a_list_of_ids_never_a_mixture() {
        let two = settings(&[(
            "PUSH_ENTRA_ALLOWED_TENANTS",
            "33333333-0000-4000-8000-000000000003, 44444444-0000-4000-8000-000000000004",
        )])
        .unwrap();
        assert!(matches!(two.tenants, Tenants::Ids(ref ids) if ids.len() == 2));
        for (value, expected) in [
            ("common", Tenants::Common),
            (" Organizations ", Tenants::Organizations),
            ("CONSUMERS", Tenants::Consumers),
        ] {
            assert_eq!(
                settings(&[("PUSH_ENTRA_ALLOWED_TENANTS", value)])
                    .unwrap()
                    .tenants,
                expected
            );
        }
        let mixed = format!("{TENANT},common");
        let twice = format!("{TENANT},{TENANT}");
        let empty_entry = format!("{TENANT},,44444444-0000-4000-8000-000000000004");
        let trailing = format!("{TENANT},");
        let many = (1..=21)
            .map(|n| format!("{n:08x}-0000-4000-8000-000000000000"))
            .collect::<Vec<_>>()
            .join(",");
        for bad in [
            mixed.as_str(),
            "common,organizations",
            "organizations,consumers",
            "consumers,consumers",
            twice.as_str(),
            empty_entry.as_str(),
            trailing.as_str(),
            "contoso.onmicrosoft.com",
            "00000000-0000-0000-0000-000000000000",
            "all",
            many.as_str(),
        ] {
            assert!(refused("PUSH_ENTRA_ALLOWED_TENANTS", bad), "{bad}");
        }
        assert_eq!(
            settings(&[("PUSH_ENTRA_ALLOWED_TENANTS", "")]),
            Err(ConfigError::Missing("PUSH_ENTRA_ALLOWED_TENANTS"))
        );
    }

    #[test]
    fn each_keyword_admits_exactly_its_tenants_and_has_one_key_source() {
        let listed = Uuid::parse_str(TENANT).unwrap();
        let work = Uuid::from_u128(0x55555555_0000_4000_8000_000000000005);
        let cases = [
            (Tenants::Common, [true, true, true], "common"),
            (Tenants::Organizations, [true, true, false], "organizations"),
            (Tenants::Consumers, [false, false, true], "consumers"),
        ];
        for (tenants, [admits_listed, admits_work, admits_personal], source) in cases {
            assert_eq!(tenants.admits(listed), admits_listed, "{tenants:?}");
            assert_eq!(tenants.admits(work), admits_work, "{tenants:?}");
            assert_eq!(
                tenants.admits(CONSUMERS_TENANT),
                admits_personal,
                "{tenants:?}"
            );
            assert!(!tenants.admits(Uuid::nil()));
            assert_eq!(tenants.key_sources(), [source]);
            let admitted = [listed, work, CONSUMERS_TENANT]
                .into_iter()
                .find(|tid| tenants.admits(*tid))
                .unwrap();
            assert_eq!(tenants.key_source(admitted).as_deref(), Some(source));
        }
        let ids = Tenants::Ids(vec![listed]);
        assert!(ids.admits(listed) && !ids.admits(work) && !ids.admits(CONSUMERS_TENANT));
        assert_eq!(ids.key_source(listed), Some(TENANT.to_owned()));
        assert_eq!(ids.key_source(work), None);
    }

    #[test]
    fn audiences_are_the_application_id_and_optionally_its_uri() {
        let both = settings(&[(
            "PUSH_ENTRA_CLIENT_ID",
            "api://push-api, 11111111-0000-4000-8000-000000000001",
        )])
        .unwrap();
        assert_eq!(both.audiences, ["api://push-api", API]);
        for bad in [
            "api://push-api",
            "https://push.example.com",
            "push",
            "api://",
            "api://a b",
            // A comma always separates audiences, each checked on its own.
            "api://a,anything",
            "api://a,11111111-0000-4000-8000-000000000001,anything",
            "api://a,api://b,api://c,api://d,11111111-0000-4000-8000-000000000001",
        ] {
            assert!(refused("PUSH_ENTRA_CLIENT_ID", bad), "{bad}");
        }
        assert_eq!(
            settings(&[("PUSH_ENTRA_CLIENT_ID", " ")]),
            Err(ConfigError::Missing("PUSH_ENTRA_CLIENT_ID"))
        );
    }

    #[test]
    fn the_authority_is_https_or_loopback_http_and_nothing_else() {
        assert_eq!(
            settings(&[(
                "PUSH_ENTRA_AUTHORITY_HOST",
                "https://login.microsoftonline.us"
            )])
            .unwrap()
            .authority_host,
            "https://login.microsoftonline.us/"
        );
        assert_eq!(
            settings(&[("PUSH_ENTRA_AUTHORITY_HOST", "http://127.0.0.1:8123/")])
                .unwrap()
                .authority_host,
            "http://127.0.0.1:8123/"
        );
        for bad in [
            "http://login.microsoftonline.com/",
            "https://login.microsoftonline.com/common",
            "https://user@login.microsoftonline.com/",
            "https://login.microsoftonline.com/?x=1",
            "login.microsoftonline.com",
        ] {
            assert!(refused("PUSH_ENTRA_AUTHORITY_HOST", bad), "{bad}");
        }
    }

    #[test]
    fn scope_role_client_apps_and_session_age_can_be_set() {
        let set = settings(&[
            ("PUSH_ENTRA_REQUIRED_SCOPE", "Chat.Use"),
            ("PUSH_ENTRA_REQUIRED_ROLE", "Messenger.User"),
            (
                "PUSH_ENTRA_ALLOWED_CLIENT_APPS",
                "22222222-0000-4000-8000-000000000002",
            ),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "8"),
        ])
        .unwrap();
        assert_eq!(set.required_scope.as_deref(), Some("Chat.Use"));
        assert_eq!(set.required_role.as_deref(), Some("Messenger.User"));
        assert_eq!(set.client_apps.len(), 1);
        assert_eq!(set.max_session, Duration::from_secs(8 * 3600));
        assert_eq!(
            settings(&[("PUSH_ENTRA_REQUIRED_SCOPE", "*")])
                .unwrap()
                .required_scope,
            None
        );
        for (name, value) in [
            ("PUSH_ENTRA_REQUIRED_SCOPE", "two scopes"),
            ("PUSH_ENTRA_REQUIRED_ROLE", "a\u{7f}"),
            ("PUSH_ENTRA_ALLOWED_CLIENT_APPS", "spa"),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "0"),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "721"),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "1.5"),
        ] {
            assert!(refused(name, value), "{name}={value}");
        }
    }
}
