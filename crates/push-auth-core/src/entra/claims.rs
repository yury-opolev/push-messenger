//! The claims of an Entra ID access token beyond those `jsonwebtoken` checks (signature,
//! `iss`, `aud`, `exp`, `nbf`): a v2.0 delegated token of the tenant its issuer named,
//! with the required scope and, when configured, the role and client app (spec D2).

use serde::Deserialize;
use uuid::Uuid;

use super::{EntraIdentity, EntraSettings, Reason};
use crate::identity::informational;

/// The claims read from an access token. A claim of the wrong JSON type makes the whole
/// token malformed. No `Debug`: it holds the name and the address.
#[derive(Clone, Default, Deserialize)]
pub(crate) struct EntraClaims {
    #[serde(default)]
    pub ver: Option<String>,
    #[serde(default)]
    pub tid: Option<String>,
    #[serde(default)]
    pub oid: Option<String>,
    #[serde(default)]
    pub scp: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub idtyp: Option<String>,
    #[serde(default)]
    pub azp: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// The identity in `claims`, of a token whose signature and issuer named `tenant`.
pub(crate) fn check(
    claims: &EntraClaims,
    tenant: Uuid,
    settings: &EntraSettings,
) -> Result<EntraIdentity, Reason> {
    if claims.ver.as_deref() != Some("2.0") {
        return Err(Reason::Version);
    }
    if claims.tid.as_deref().and_then(guid) != Some(tenant) {
        return Err(Reason::Tenant);
    }
    let scopes: Vec<&str> = claims
        .scp
        .as_deref()
        .unwrap_or_default()
        .split(' ')
        .filter(|scope| !scope.is_empty())
        .collect();
    // Delegated tokens only: an app-only token has no scp (and `idtyp: app` when that
    // optional claim is configured); an ID token has none either.
    if claims.idtyp.as_deref() == Some("app") || scopes.is_empty() {
        return Err(Reason::AppOnly);
    }
    if let Some(required) = &settings.required_scope {
        if !scopes.contains(&required.as_str()) {
            return Err(Reason::Scope);
        }
    }
    if let Some(role) = &settings.required_role {
        if !claims.roles.iter().any(|held| held == role) {
            return Err(Reason::Role);
        }
    }
    if !settings.client_apps.is_empty()
        && !claims
            .azp
            .as_deref()
            .and_then(guid)
            .is_some_and(|app| settings.client_apps.contains(&app))
    {
        return Err(Reason::ClientApp);
    }
    let object_id = claims
        .oid
        .as_deref()
        .and_then(guid)
        .ok_or(Reason::Subject)?;
    Ok(EntraIdentity {
        tenant_id: tenant,
        object_id,
        name: informational(claims.name.as_deref()),
        email: informational(claims.email.as_deref())
            .or_else(|| informational(claims.preferred_username.as_deref())),
    })
}

fn guid(value: &str) -> Option<Uuid> {
    Uuid::try_parse(value).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::entra::Tenants;

    const TENANT: Uuid = Uuid::from_u128(0x33333333_0000_4000_8000_000000000003);
    const USER: &str = "55555555-0000-4000-8000-000000000005";
    const SPA: &str = "22222222-0000-4000-8000-000000000002";

    fn settings() -> EntraSettings {
        EntraSettings {
            authority_host: "https://login.microsoftonline.com/".to_owned(),
            tenants: Tenants::Ids(vec![TENANT]),
            audiences: vec!["11111111-0000-4000-8000-000000000001".to_owned()],
            required_scope: Some("access_as_user".to_owned()),
            required_role: None,
            client_apps: Vec::new(),
            max_session: Duration::from_secs(3600),
        }
    }

    fn delegated() -> EntraClaims {
        EntraClaims {
            ver: Some("2.0".to_owned()),
            tid: Some(TENANT.to_string()),
            oid: Some(USER.to_owned()),
            scp: Some("access_as_user".to_owned()),
            azp: Some(SPA.to_owned()),
            name: Some(" Alice Smith ".to_owned()),
            preferred_username: Some("alice@contoso.test".to_owned()),
            ..EntraClaims::default()
        }
    }

    #[test]
    fn a_delegated_v2_token_names_its_user() {
        let identity = check(&delegated(), TENANT, &settings()).unwrap();
        assert_eq!(identity.tenant_id, TENANT);
        assert_eq!(identity.object_id.to_string(), USER);
        assert_eq!(identity.name.as_deref(), Some("Alice Smith"));
        assert_eq!(identity.email.as_deref(), Some("alice@contoso.test"));
        let with_email = EntraClaims {
            email: Some("a.smith@contoso.test".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&with_email, TENANT, &settings())
                .unwrap()
                .email
                .as_deref(),
            Some("a.smith@contoso.test")
        );
        let bare = EntraClaims {
            name: None,
            preferred_username: Some("\u{7}".to_owned()),
            ..delegated()
        };
        let identity = check(&bare, TENANT, &settings()).unwrap();
        assert_eq!((identity.name, identity.email), (None, None));
    }

    #[test]
    fn other_versions_tenants_and_subjects_are_refused() {
        let cases = [
            (
                EntraClaims {
                    ver: Some("1.0".to_owned()),
                    ..delegated()
                },
                Reason::Version,
            ),
            (
                EntraClaims {
                    ver: None,
                    ..delegated()
                },
                Reason::Version,
            ),
            (
                EntraClaims {
                    tid: Some("44444444-0000-4000-8000-000000000004".to_owned()),
                    ..delegated()
                },
                Reason::Tenant,
            ),
            (
                EntraClaims {
                    tid: None,
                    ..delegated()
                },
                Reason::Tenant,
            ),
            (
                EntraClaims {
                    oid: None,
                    ..delegated()
                },
                Reason::Subject,
            ),
            (
                EntraClaims {
                    oid: Some("not-a-guid".to_owned()),
                    ..delegated()
                },
                Reason::Subject,
            ),
        ];
        for (claims, reason) in cases {
            assert_eq!(check(&claims, TENANT, &settings()).err(), Some(reason));
        }
    }

    #[test]
    fn app_only_and_id_tokens_lack_a_delegated_scope() {
        // App-only (client credentials): roles, no scp; and with the optional idtyp claim.
        let app = EntraClaims {
            scp: None,
            roles: vec!["Messages.Send".to_owned()],
            ..delegated()
        };
        assert_eq!(
            check(&app, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        let typed = EntraClaims {
            idtyp: Some("app".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&typed, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        // An ID token has no scp either (its audience, the client app, fails first).
        let id_token = EntraClaims {
            scp: None,
            ..delegated()
        };
        assert_eq!(
            check(&id_token, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        let blank = EntraClaims {
            scp: Some("  ".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&blank, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        let user = EntraClaims {
            idtyp: Some("user".to_owned()),
            ..delegated()
        };
        assert!(check(&user, TENANT, &settings()).is_ok());
    }

    #[test]
    fn the_required_scope_role_and_client_app_hold() {
        let other = EntraClaims {
            scp: Some("User.Read".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&other, TENANT, &settings()).err(),
            Some(Reason::Scope)
        );
        let several = EntraClaims {
            scp: Some("User.Read access_as_user".to_owned()),
            ..delegated()
        };
        assert!(check(&several, TENANT, &settings()).is_ok());
        let any = EntraSettings {
            required_scope: None,
            ..settings()
        };
        assert!(check(&other, TENANT, &any).is_ok());

        let role = EntraSettings {
            required_role: Some("Messenger.User".to_owned()),
            ..settings()
        };
        assert_eq!(check(&delegated(), TENANT, &role).err(), Some(Reason::Role));
        let member = EntraClaims {
            roles: vec!["Messenger.User".to_owned()],
            ..delegated()
        };
        assert!(check(&member, TENANT, &role).is_ok());

        let apps = EntraSettings {
            client_apps: vec![Uuid::parse_str(SPA).unwrap()],
            ..settings()
        };
        assert!(check(&delegated(), TENANT, &apps).is_ok());
        for azp in [
            None,
            Some("99999999-0000-4000-8000-000000000009".to_owned()),
        ] {
            let elsewhere = EntraClaims { azp, ..delegated() };
            assert_eq!(
                check(&elsewhere, TENANT, &apps).err(),
                Some(Reason::ClientApp)
            );
        }
    }

    #[test]
    fn the_identity_never_shows_its_object_id_name_or_email() {
        let shown = format!("{:?}", check(&delegated(), TENANT, &settings()).unwrap());
        for secret in [USER, "Alice", "alice@contoso.test"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains(&TENANT.to_string()), "{shown}");
    }
}
