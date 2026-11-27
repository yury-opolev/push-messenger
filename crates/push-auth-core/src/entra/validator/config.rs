//! The settings of an external token validator: `PUSH_ENTRA_VALIDATOR_*`.

use std::time::Duration;

use reqwest::Url;

use crate::config::ConfigError;

const URL_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_URL";
const TIMEOUT_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_TIMEOUT_MS";
const SECRET_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_SECRET_FILE";
const DEFAULT_TIMEOUT_MS: u64 = 2000;
const MAX_SECRET_BYTES: usize = 1024;

/// Where and how to reach an external token validator. `Debug` never shows the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ValidatorSettings {
    /// The endpoint: `https://`, or `http://` to a loopback host.
    pub url: String,
    /// The whole request's time limit, connecting included.
    pub timeout: Duration,
    /// Sent as `X-Push-Validator-Secret` when set.
    pub secret: Option<String>,
}

impl std::fmt::Debug for ValidatorSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatorSettings")
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ValidatorSettings {
    /// Reads `PUSH_ENTRA_VALIDATOR_*`: `None` when the URL is unset or empty. Errors name
    /// the variable; they show the secret file's path, never its content, and no URL with
    /// user info.
    pub(in crate::entra) fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
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
        let timeout = get(TIMEOUT_VARIABLE);
        let secret_file = get(SECRET_VARIABLE);
        let Some(url) = get(URL_VARIABLE) else {
            let only_with_url = "set only with PUSH_ENTRA_VALIDATOR_URL";
            if let Some(timeout) = timeout {
                return Err(invalid(TIMEOUT_VARIABLE, &timeout, only_with_url));
            }
            if let Some(path) = secret_file {
                return Err(invalid(SECRET_VARIABLE, &path, only_with_url));
            }
            return Ok(None);
        };
        let url = validator_url(&url).map_err(|(shown, reason)| {
            invalid(
                URL_VARIABLE,
                if shown { &url } else { "<redacted>" },
                reason,
            )
        })?;
        let timeout = match timeout {
            None => Duration::from_millis(DEFAULT_TIMEOUT_MS),
            Some(value) => value
                .parse::<u64>()
                .ok()
                .filter(|millis| (100..=10_000).contains(millis))
                .map(Duration::from_millis)
                .ok_or_else(|| {
                    invalid(
                        TIMEOUT_VARIABLE,
                        &value,
                        "use a whole number of milliseconds from 100 to 10000",
                    )
                })?,
        };
        let secret = match secret_file {
            None => None,
            Some(path) => {
                Some(read_secret(&path).map_err(|reason| invalid(SECRET_VARIABLE, &path, reason))?)
            }
        };
        Ok(Some(Self {
            url,
            timeout,
            secret,
        }))
    }
}

/// The validator endpoint, or whether the value may be shown and why it is refused.
fn validator_url(value: &str) -> Result<String, (bool, &'static str)> {
    const REASON: &str = "must be an https:// URL (http:// only for a loopback host: \
                          127.0.0.1, [::1], localhost), without user info or fragment";
    let url = Url::parse(value).map_err(|_| (!value.contains('@'), REASON))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err((false, REASON));
    }
    let scheme_ok = match url.scheme() {
        "https" => true,
        "http" => is_loopback(&url),
        _ => false,
    };
    if !scheme_ok || url.host_str().is_none() || url.fragment().is_some() {
        return Err((true, REASON));
    }
    Ok(url.to_string())
}

pub(super) fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
}

/// The trimmed content of the secret file.
fn read_secret(path: &str) -> Result<String, &'static str> {
    let content = std::fs::read_to_string(path).map_err(|_| "cannot be read")?;
    let secret = content.trim();
    if secret.is_empty() {
        return Err("the file is empty");
    }
    if secret.len() > MAX_SECRET_BYTES || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("the secret must be 1 to 1024 visible ASCII characters");
    }
    Ok(secret.to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::entra::EntraSettings;

    const TENANT: &str = "33333333-0000-4000-8000-000000000003";
    const API: &str = "11111111-0000-4000-8000-000000000001";
    const URL: &str = "PUSH_ENTRA_VALIDATOR_URL";
    const TIMEOUT: &str = "PUSH_ENTRA_VALIDATOR_TIMEOUT_MS";
    const SECRET: &str = "PUSH_ENTRA_VALIDATOR_SECRET_FILE";

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

    fn validator(variables: &[(&str, &str)]) -> Result<Option<ValidatorSettings>, ConfigError> {
        settings(variables).map(|settings| settings.validator)
    }

    fn refused_as(variables: &[(&str, &str)], name: &str) -> ConfigError {
        let error = validator(variables).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Invalid { name: refused, .. } if *refused == name),
            "{error:?}"
        );
        error
    }

    /// A file of the system's temporary directory, removed when dropped.
    struct SecretFile(PathBuf);

    impl SecretFile {
        fn new(content: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("push-validator-{}.secret", Uuid::now_v7()));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for SecretFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn without_a_url_there_is_no_validator() {
        assert_eq!(validator(&[]).unwrap(), None);
        assert_eq!(validator(&[(URL, "  ")]).unwrap(), None);
    }

    #[test]
    fn the_url_is_https_or_loopback_http_with_a_path_and_query_allowed() {
        for (value, expected) in [
            (
                "http://127.0.0.1:9000/validate",
                "http://127.0.0.1:9000/validate",
            ),
            ("http://[::1]:9000/v", "http://[::1]:9000/v"),
            ("http://localhost:9000", "http://localhost:9000/"),
            (
                " https://validator.example/api/check?tier=1 ",
                "https://validator.example/api/check?tier=1",
            ),
        ] {
            let found = validator(&[(URL, value)]).unwrap().unwrap();
            assert_eq!(found.url, expected);
            assert_eq!(found.timeout, Duration::from_millis(2000));
            assert_eq!(found.secret, None);
        }
        for bad in [
            "http://validator.example/",
            "ftp://127.0.0.1/",
            "https://h.example/#f",
            "not a url",
            "/validate",
        ] {
            refused_as(&[(URL, bad)], URL);
        }
        let error = refused_as(&[(URL, "https://u:p@h.example/")], URL);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("p@") && !shown.contains("u:p"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        refused_as(&[(URL, "https://user@h.example/")], URL);
    }

    #[test]
    fn an_unparsable_url_with_user_info_is_not_shown() {
        let error = refused_as(&[(URL, "https://user:s3cret@host:99999/")], URL);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(
            shown.contains(URL) && shown.contains("<redacted>"),
            "{shown}"
        );
        let error = refused_as(&[(URL, "https://host:99999/")], URL);
        assert!(
            format!("{error}").contains("https://host:99999/"),
            "{error}"
        );
    }

    #[test]
    fn the_timeout_is_100_to_10000_milliseconds() {
        let url = (URL, "http://127.0.0.1:9000/validate");
        for (value, millis) in [("100", 100), ("10000", 10000), (" 2500 ", 2500)] {
            let found = validator(&[url, (TIMEOUT, value)]).unwrap().unwrap();
            assert_eq!(found.timeout, Duration::from_millis(millis));
        }
        for bad in ["99", "10001", "1.5", "x", "-1"] {
            refused_as(&[url, (TIMEOUT, bad)], TIMEOUT);
        }
    }

    #[test]
    fn the_secret_is_read_from_a_file_trimmed_and_never_shown() {
        let url = (URL, "http://127.0.0.1:9000/validate");
        let file = SecretFile::new("  s3cret \n");
        let found = validator(&[url, (SECRET, file.path())]).unwrap().unwrap();
        assert_eq!(found.secret.as_deref(), Some("s3cret"));
        let entra = settings(&[url, (SECRET, file.path())]).unwrap();
        let shown = format!("{entra:?}");
        assert!(
            !shown.contains("s3cret") && shown.contains("<redacted>"),
            "{shown}"
        );

        let blank = SecretFile::new(" \n\t ");
        refused_as(&[url, (SECRET, blank.path())], SECRET);
        let spaced = SecretFile::new("two words");
        let error = refused_as(&[url, (SECRET, spaced.path())], SECRET);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("two words"), "{shown}");
        assert!(matches!(error, ConfigError::Invalid { value, .. } if value == spaced.path()));
        let long = SecretFile::new(&"a".repeat(1025));
        let error = refused_as(&[url, (SECRET, long.path())], SECRET);
        assert!(!format!("{error}").contains(&"a".repeat(100)));
        let missing =
            std::env::temp_dir().join(format!("push-validator-{}.missing", Uuid::now_v7()));
        refused_as(&[url, (SECRET, missing.to_str().unwrap())], SECRET);
    }

    #[test]
    fn the_timeout_and_secret_are_refused_without_a_url() {
        let file = SecretFile::new("s3cret");
        let error = refused_as(&[(TIMEOUT, "2000")], TIMEOUT);
        assert!(format!("{error}").contains(URL), "{error}");
        let error = refused_as(&[(SECRET, file.path())], SECRET);
        assert!(!format!("{error}").contains("s3cret"), "{error}");
        assert_eq!(validator(&[(TIMEOUT, " "), (SECRET, "")]).unwrap(), None);
    }
}
