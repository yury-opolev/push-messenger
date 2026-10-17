//! The operator's test accounts: JSON Lines, one `{"email": "...", "password": "..."}`
//! per line. They must be verified, dedicated test accounts: push-load never creates
//! accounts against a deployment.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

pub const MAX_ACCOUNTS: usize = 2_000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub email: String,
    pub password: String,
}

// Neither the address nor the password belongs in output.
impl std::fmt::Debug for Account {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Account { .. }")
    }
}

/// Parses the accounts; errors name the line, never its content.
pub fn parse(text: &str) -> anyhow::Result<Vec<Account>> {
    let mut accounts = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        // serde_json's message can quote the input, so it is not shown.
        let account: Account = serde_json::from_str(line).map_err(|_| {
            anyhow::anyhow!(
                r#"accounts line {number}: expected {{"email": "...", "password": "..."}}"#
            )
        })?;
        if account.email.trim().is_empty() || account.password.is_empty() {
            bail!("accounts line {number}: email and password must not be empty");
        }
        if !seen.insert(account.email.trim().to_lowercase()) {
            bail!("accounts line {number}: the same email appears twice");
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

pub fn read(path: &Path) -> anyhow::Result<Vec<Account>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the accounts file {}", path.display()))?;
    parse(&text)
}

/// The accounts as JSON Lines.
pub fn to_lines(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| serde_json::to_string(account).expect("an account serializes") + "\n")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_round_trip_and_blank_lines_are_skipped() {
        let accounts = parse("\n{\"email\":\"a@x.test\",\"password\":\"pw-a\"}\n\n{\"email\":\"b@x.test\",\"password\":\"pw-b\"}\n").unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(parse(&to_lines(&accounts)).unwrap(), accounts);
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
