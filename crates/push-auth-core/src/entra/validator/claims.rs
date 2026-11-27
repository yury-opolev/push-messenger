//! The schema of a validator's claims and their defensive re-checks.

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::entra::{EntraSettings, LEEWAY_SECONDS, Reason};

fn is_string(value: &Value) -> bool {
    value.is_string()
}

fn is_integer(value: &Value) -> bool {
    value.is_i64() || value.is_u64()
}

fn is_strings(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(Value::is_string))
}

fn is_audience(value: &Value) -> bool {
    is_string(value) || is_strings(value)
}

/// A claim's name and its JSON type check.
type ClaimRule = (&'static str, fn(&Value) -> bool);

/// Whether `claims` follow the response schema: the required claims present, and every
/// known claim, when present, of its JSON type (`null` is no string).
fn follows_schema(claims: &Map<String, Value>) -> bool {
    const REQUIRED: [ClaimRule; 6] = [
        ("ver", is_string),
        ("iss", is_string),
        ("tid", is_string),
        ("oid", is_string),
        ("aud", is_audience),
        ("exp", is_integer),
    ];
    const OPTIONAL: [ClaimRule; 11] = [
        ("sub", is_string),
        ("azp", is_string),
        ("appid", is_string),
        ("scp", is_string),
        ("idtyp", is_string),
        ("name", is_string),
        ("preferred_username", is_string),
        ("email", is_string),
        ("roles", is_strings),
        ("nbf", is_integer),
        ("iat", is_integer),
    ];
    REQUIRED
        .iter()
        .all(|(name, valid)| claims.get(*name).is_some_and(valid))
        && OPTIONAL
            .iter()
            .all(|(name, valid)| claims.get(*name).is_none_or(valid))
}

/// An integer claim as seconds; one beyond `i64` is far in the future.
fn seconds(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value.as_i64().or_else(|| value.as_u64().map(|_| i64::MAX))
}

/// The defensive re-checks of a validator's `claims` for a token whose unverified payload
/// named `token_tid` and `token_oid`: a validator can only narrow who signs in.
pub(crate) fn recheck(
    claims: &Map<String, Value>,
    token_tid: Option<&str>,
    token_oid: Option<&str>,
    tenant: Uuid,
    settings: &EntraSettings,
    now: i64,
) -> Result<(), Reason> {
    if !follows_schema(claims) {
        return Err(Reason::Malformed);
    }
    let text = |name: &str| claims.get(name).and_then(Value::as_str);
    let tid = text("tid");
    if token_tid.is_none() || tid != token_tid || token_oid.is_none() || text("oid") != token_oid {
        return Err(Reason::Malformed);
    }
    if !tid
        .and_then(|tid| Uuid::try_parse(tid).ok())
        .is_some_and(|tid| tid == tenant && settings.tenants.admits(tid))
    {
        return Err(Reason::Tenant);
    }
    if text("iss") != Some(settings.issuer(tenant).as_str()) {
        return Err(Reason::Issuer);
    }
    let configured = |audience: &Value| {
        audience
            .as_str()
            .is_some_and(|audience| settings.audiences.iter().any(|known| known == audience))
    };
    let audience_ok = match claims.get("aud") {
        Some(Value::Array(audiences)) => audiences.iter().any(configured),
        Some(audience) => configured(audience),
        None => false,
    };
    if !audience_ok {
        return Err(Reason::Audience);
    }
    let leeway = LEEWAY_SECONDS as i64;
    if seconds(claims.get("exp")).is_none_or(|exp| exp < now - leeway) {
        return Err(Reason::Expired);
    }
    if seconds(claims.get("nbf")).is_some_and(|nbf| nbf > now + leeway) {
        return Err(Reason::NotYetValid);
    }
    Ok(())
}
