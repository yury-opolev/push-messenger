//! Which credentials a datastore takes.

/// `password` (a URL with the password in it, the default) or `azure-entra` (Microsoft
/// Entra ID access tokens from Azure workload identity or managed identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    #[default]
    Password,
    AzureEntra,
}

impl AuthMode {
    /// Parses a `PUSH_POSTGRES_AUTH` / `PUSH_REDIS_AUTH` value; unset or empty is
    /// `password`.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("password") => Ok(Self::Password),
            Some("azure-entra") => Ok(Self::AzureEntra),
            Some(_) => Err("expected password or azure-entra".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_password_and_only_two_modes_exist() {
        assert_eq!(AuthMode::parse(None), Ok(AuthMode::Password));
        assert_eq!(AuthMode::parse(Some(" ")), Ok(AuthMode::Password));
        assert_eq!(AuthMode::parse(Some("password")), Ok(AuthMode::Password));
        assert_eq!(
            AuthMode::parse(Some("azure-entra")),
            Ok(AuthMode::AzureEntra)
        );
        assert!(AuthMode::parse(Some("entra")).is_err());
        assert!(AuthMode::parse(Some("Azure-Entra")).is_err());
    }
}
