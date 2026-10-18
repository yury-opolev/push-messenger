//! Microsoft Entra ID sign-in (spec 2026-10-19): auth-svc checks an Entra ID access token
//! issued for its API and finds the user by the token's immutable `(tid, oid)`. It never
//! redeems authorization codes and holds no client secret. Tokens, object ids, names and
//! email addresses are never logged.

mod settings;

pub use settings::{
    CONSUMERS_TENANT, DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers,
    Tenants,
};
