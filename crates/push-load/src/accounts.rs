//! Operator accounts: password credentials or an Entra token (inline or reread from a
//! file). Entra tenant/object claims identify state entries; only the server validates
//! tokens. No operator token is serializable or included in diagnostics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine;
use serde::Deserialize;
use uuid::Uuid;

pub const MAX_ACCOUNTS: usize = 2_000;

#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    /// Stable state key: an email for passwords, `entra:<tenant>:<object>` for Entra.
    pub email: String,
    pub password: String,
    entra: Option<TokenSource>,
}

#[derive(Clone, PartialEq, Eq)]
enum TokenSource {
    Inline(String),
    File(PathBuf),
}

impl Account {
    pub fn password(email: String, password: String) -> Self {
        Self {
            email,
            password,
            entra: None,
        }
    }

    pub fn is_entra(&self) -> bool {
        self.entra.is_some()
    }

    /// Rereads file sources every time, refusing an identity change before any request.
    pub fn entra_token(&self) -> anyhow::Result<Option<String>> {
        let Some(source) = &self.entra else {
            return Ok(None);
        };
        let token = source.read()?;
        if identity(&token)? != self.email {
            bail!("the Entra token source changed identity; restore the original identity's token");
        }
        Ok(Some(token))
    }
}

impl TokenSource {
    fn read(&self) -> anyhow::Result<String> {
        let token = match self {
            Self::Inline(token) => token.clone(),
            Self::File(path) => std::fs::read_to_string(path)
                .map_err(|_| anyhow::anyhow!("could not read the Entra token file"))?,
        };
        let token = token.trim();
        if token.is_empty() {
            bail!("the Entra token must not be empty");
        }
        Ok(token.to_owned())
    }
}

fn identity(token: &str) -> anyhow::Result<String> {
    let invalid = || anyhow::anyhow!("expected an Entra JWT with UUID tid and oid claims");
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(invalid());
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| invalid())?;
    #[derive(Deserialize)]
    struct Claims {
        tid: Uuid,
        oid: Uuid,
    }
    let claims: Claims = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    Ok(format!("entra:{}:{}", claims.tid, claims.oid))
}

// Neither the address nor the password belongs in output.
impl std::fmt::Debug for Account {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Account { .. }")
    }
}

/// Parses the accounts; errors name the line, never its content.
pub fn parse(text: &str) -> anyhow::Result<Vec<Account>> {
    parse_at(text, Path::new("."))
}

fn parse_at(text: &str, directory: &Path) -> anyhow::Result<Vec<Account>> {
    let mut accounts = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        // serde_json's message can quote the input, so it is not shown.
        let account = parse_line(line, directory)
            .map_err(|error| anyhow::anyhow!("accounts line {number}: {error}"))?;
        if !seen.insert(account.email.trim().to_lowercase()) {
            bail!("accounts line {number}: the same account appears twice");
        }
        accounts.push(account);
        if accounts.len() > MAX_ACCOUNTS {
            bail!("the accounts file holds more than {MAX_ACCOUNTS} accounts");
        }
    }
    if accounts.is_empty() {
        bail!("the accounts file holds no accounts");
    }
    Ok(accounts)
}

fn parse_line(line: &str, directory: &Path) -> anyhow::Result<Account> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        email: Option<String>,
        password: Option<String>,
        entra_token: Option<String>,
        entra_token_file: Option<String>,
    }
    let invalid = || {
        anyhow::anyhow!(
            "expected exactly one of {{email, password}}, {{entra_token}} or {{entra_token_file}}, with nonempty strings"
        )
    };
    let input: Input = serde_json::from_str(line).map_err(|_| invalid())?;
    // Count present keys, including null: an ambiguous source must never be ignored.
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| invalid())?;
    let keys = value.as_object().ok_or_else(invalid)?;
    if let (Some(email), Some(password)) = (input.email, input.password) {
        if keys.len() != 2 || email.trim().is_empty() || password.trim().is_empty() {
            return Err(invalid());
        }
        return Ok(Account::password(email.trim().to_owned(), password));
    }
    if keys.len() != 1 {
        return Err(invalid());
    }
    let source = match (input.entra_token, input.entra_token_file) {
        (Some(token), None) => TokenSource::Inline(token),
        (None, Some(file)) if !file.trim().is_empty() => TokenSource::File(directory.join(file)),
        _ => return Err(invalid()),
    };
    let email = identity(&source.read()?)?;
    Ok(Account {
        email,
        password: String::new(),
        entra: Some(source),
    })
}

pub fn read(path: &Path) -> anyhow::Result<Vec<Account>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the accounts file {}", path.display()))?;
    parse_at(&text, path.parent().unwrap_or(Path::new(".")))
}

/// The accounts as JSON Lines.
pub fn to_lines(accounts: &[Account]) -> anyhow::Result<String> {
    accounts
        .iter()
        .map(|account| {
            if account.is_entra() {
                bail!("operator Entra tokens cannot be exported");
            }
            Ok(
                serde_json::json!({"email": account.email, "password": account.password})
                    .to_string()
                    + "\n",
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    pub(crate) fn token(oid: &str, nonce: &str) -> String {
        let payload = serde_json::json!({
            "tid": "11111111-1111-1111-1111-111111111111",
            "oid": oid,
            "nonce": nonce,
        });
        format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    #[test]
    fn entra_inputs_are_distinct_and_redacted() {
        let a = token("22222222-2222-2222-2222-222222222222", "first");
        let b = token("33333333-3333-3333-3333-333333333333", "second");
        let accounts = parse(&format!(
            "{{\"entra_token\":\"{a}\"}}\n{{\"entra_token\":\"{b}\"}}"
        ))
        .unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(!format!("{accounts:?}").contains(&a));
        assert!(
            to_lines(&accounts).is_err(),
            "operator tokens must never be exported"
        );
        let rotated = token("22222222-2222-2222-2222-222222222222", "rotated");
        assert!(
            parse(&format!(
                "{{\"entra_token\":\"{a}\"}}\n{{\"entra_token\":\"{rotated}\"}}"
            ))
            .is_err()
        );
        for input in [
            format!("{{\"email\":\"a@x.test\",\"password\":\"pw\",\"entra_token\":\"{a}\"}}"),
            format!("{{\"entra_token\":\"{a}\",\"entra_token_file\":\"token.txt\"}}"),
            "{\"entra_token\":\" \"}".to_owned(),
            "{\"entra_token_file\":\"\"}".to_owned(),
            "{\"entra_token\":\"not-a-jwt-secret\"}".to_owned(),
            format!("{{\"entra_token\":\"{a}\",\"password\":null}}"),
            format!("{{\"entra_token\":\"{a}\",\"entra_token\":\"{b}\"}}"),
        ] {
            let error = parse(&input).unwrap_err().to_string();
            assert!(error.contains("line 1"));
            assert!(!error.contains(&a) && !error.contains("not-a-jwt-secret"));
        }
    }

    #[test]
    fn token_files_reject_identity_changes_and_unreadable_or_empty_contents() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join(format!(".push-account-input-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.jsonl");
        let file = dir.join("token.jwt");
        let a = token("22222222-2222-2222-2222-222222222222", "first");
        std::fs::write(&file, &a).unwrap();
        std::fs::write(&path, "{\"entra_token_file\":\"token.jwt\"}").unwrap();
        let accounts = read(&path).unwrap();
        assert_eq!(
            accounts[0].entra_token().unwrap().as_deref(),
            Some(a.as_str())
        );
        let other = token("33333333-3333-3333-3333-333333333333", "other");
        std::fs::write(&file, &other).unwrap();
        let error = accounts[0].entra_token().unwrap_err().to_string();
        assert!(error.contains("changed identity") && !error.contains(&other));
        std::fs::write(&file, " \n").unwrap();
        assert!(accounts[0].entra_token().is_err());
        std::fs::remove_file(&file).unwrap();
        assert!(accounts[0].entra_token().is_err());
        assert!(read(&path).unwrap_err().to_string().contains("line 1"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn accounts_round_trip_and_blank_lines_are_skipped() {
        let accounts = parse("\n{\"email\":\"a@x.test\",\"password\":\"pw-a\"}\n\n{\"email\":\"b@x.test\",\"password\":\"pw-b\"}\n").unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(parse(&to_lines(&accounts).unwrap()).unwrap(), accounts);
        assert_eq!(format!("{:?}", accounts[0]), "Account { .. }");
    }

    #[test]
    fn bad_lines_are_named_without_their_content() {
        let error = parse("{\"email\":\"a@x.test\",\"password\":\"hunter2\",\"extra\":1}")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("line 1") && !error.contains("hunter2"),
            "{error}"
        );
        let duplicate = "{\"email\":\"a@x.test\",\"password\":\"1\"}\n{\"email\":\"A@X.test\",\"password\":\"2\"}";
        assert!(parse(duplicate).unwrap_err().to_string().contains("line 2"));
        assert!(parse("{\"email\":\"\",\"password\":\"x\"}").is_err());
        assert!(parse("\n\n").is_err());
    }
}
