//! Startup-only application keys, separate from datastore authentication.

use crate::ConfigProblem;

/// Reads an application secret from `name` or its `_FILE` alternative.
///
/// Direct values are unchanged (an empty value remains unset). Files must contain
/// nonempty UTF-8; only terminal CR/LF bytes are removed. Both variables being set,
/// even to empty strings, is an error. Errors never include values or file paths.
/// Files are read once at startup; replacing a mounted key requires a rollout.
pub fn application_secret(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<Option<String>, ConfigProblem> {
    let value = lookup(name);
    let file = lookup(&format!("{name}_FILE"));
    let invalid = |reason: &str| ConfigProblem::Invalid {
        name,
        reason: reason.to_owned(),
    };
    match (value, file) {
        (Some(_), Some(_)) => Err(invalid("set the value or its _FILE alternative, not both")),
        (value, None) => Ok(value.filter(|value| !value.is_empty())),
        (None, Some(path)) => {
            let contents = std::fs::read_to_string(path)
                .map_err(|_| invalid("_FILE must name a readable UTF-8 file"))?;
            let value = contents.trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                return Err(invalid("_FILE must contain a nonempty secret"));
            }
            Ok(Some(value.to_owned()))
        }
    }
}

/// Like [`application_secret`], but absence is an error.
pub fn required_application_secret(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigProblem> {
    application_secret(lookup, name)?.ok_or(ConfigProblem::Missing(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "PUSH_APPLICATION_KEY";

    #[test]
    fn direct_values_are_unchanged_and_absence_keeps_optional_semantics() {
        for value in ["base64==", " spaces \n", "\r\n", ""] {
            let lookup = |name: &str| (name == NAME).then(|| value.to_owned());
            assert_eq!(
                application_secret(&lookup, NAME).unwrap(),
                (!value.is_empty()).then(|| value.to_owned())
            );
        }
        assert_eq!(application_secret(&|_| None, NAME).unwrap(), None);
        assert_eq!(
            required_application_secret(&|_| None, NAME),
            Err(ConfigProblem::Missing(NAME))
        );
        for value in ["", "sensitive-direct-value"] {
            let error = application_secret(&|_| Some(value.to_owned()), NAME).unwrap_err();
            assert!(!format!("{error:?}").contains("sensitive-direct-value"));
        }
    }

    #[test]
    fn files_remove_terminal_newlines_only_and_never_encode_or_decode() {
        let directory = std::path::PathBuf::from("target")
            .join(format!("credentials-mounted-keys-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("key");
        let lookup = |name: &str| {
            (name == "PUSH_APPLICATION_KEY_FILE").then(|| file.to_str().unwrap().to_owned())
        };
        for (contents, expected) in [
            ("base64==", "base64=="),
            ("base64==\n", "base64=="),
            ("base64==\r\n\r\n", "base64=="),
            (
                " leading and trailing space \t\r\n",
                " leading and trailing space \t",
            ),
            ("embedded\nnewline\n", "embedded\nnewline"),
        ] {
            std::fs::write(&file, contents).unwrap();
            assert_eq!(
                application_secret(&lookup, NAME).unwrap().as_deref(),
                Some(expected)
            );
        }
        for contents in [b"".as_slice(), b"\r\n", &[0xff, 0xfe]] {
            std::fs::write(&file, contents).unwrap();
            assert!(application_secret(&lookup, NAME).is_err());
        }
        std::fs::remove_file(&file).unwrap();
        assert!(application_secret(&lookup, NAME).is_err());
        std::fs::create_dir(&file).unwrap();
        assert!(application_secret(&lookup, NAME).is_err());
        assert!(application_secret(&|_| Some(String::new()), NAME).is_err());
        assert!(
            application_secret(
                &|name| (name == "PUSH_APPLICATION_KEY_FILE").then(String::new),
                NAME,
            )
            .is_err()
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
