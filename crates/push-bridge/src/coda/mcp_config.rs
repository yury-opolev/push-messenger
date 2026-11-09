//! The `.mcp.json` the bridge gives Coda (spec 2026-11-09 proactive agents §8): Coda loads
//! its user-layer MCP servers from `$CODA_USER_MCP_DIR/.mcp.json` (else
//! `~/.coda/.mcp.json`), so the bridge writes `<bridge dir>/coda-mcp/.mcp.json` with the
//! user's own servers plus `push` (this program's `tools-mcp`, with the tools endpoint and
//! the run's token) and points `CODA_USER_MCP_DIR` there for the Coda it starts.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::tools::{ENDPOINT_ENV, TOKEN_ENV, mcp::SERVER_NAME};

/// Coda's override of its user-layer MCP folder.
pub const USER_MCP_DIR_ENV: &str = "CODA_USER_MCP_DIR";
/// The file name in each layer's folder.
pub const MCP_FILE: &str = ".mcp.json";
/// The folder in the bridge's folder the bridge writes Coda's user layer to.
pub const MCP_DIR: &str = "coda-mcp";

/// What Coda needs to reach the bridge's tools endpoint. `Debug` hides the token.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolsLink {
    /// The endpoint as `tools-mcp` reads it (a pipe name or a socket path).
    pub endpoint: String,
    /// The run's token.
    pub token: String,
    /// The bridge program, which serves `tools-mcp`.
    pub exe: PathBuf,
    /// Where the `.mcp.json` is written: `<bridge dir>/coda-mcp`.
    pub dir: PathBuf,
}

impl fmt::Debug for ToolsLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolsLink")
            .field("endpoint", &self.endpoint)
            .field("exe", &self.exe)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// The user-layer file Coda would read: `$CODA_USER_MCP_DIR/.mcp.json` when that is set
/// (and not empty), else `<home>/.coda/.mcp.json`; `None` without either.
pub fn user_mcp_file(user_mcp_dir: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match user_mcp_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join(MCP_FILE)),
        None => home.map(|home| home.join(".coda").join(MCP_FILE)),
    }
}

/// [`user_mcp_file`] from the bridge's environment.
pub fn current_user_mcp_file() -> Option<PathBuf> {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    user_mcp_file(
        std::env::var_os(USER_MCP_DIR_ENV),
        std::env::var_os(home)
            .filter(|home| !home.is_empty())
            .map(PathBuf::from),
    )
}

/// The `push` server's entry.
fn push_server(link: &ToolsLink) -> Value {
    json!({
        "command": link.exe.to_string_lossy(),
        "args": ["tools-mcp"],
        "env": { ENDPOINT_ENV: link.endpoint, TOKEN_ENV: link.token },
    })
}

/// The document to write: the user's (every entry and server kept as it is) with `push`
/// set; `true` when it replaced a server of the user's named `push`.
fn merged(user: Option<Value>, link: &ToolsLink) -> (Value, bool) {
    let mut document = match user {
        Some(Value::Object(document)) => document,
        _ => Map::new(),
    };
    let servers = document
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        *servers = Value::Object(Map::new());
    }
    let servers = servers.as_object_mut().expect("an object");
    let replaced = servers
        .insert(SERVER_NAME.to_owned(), push_server(link))
        .is_some();
    (Value::Object(document), replaced)
}

/// The user's servers from `file`; `None` (logged) when it is missing or not a usable
/// `.mcp.json`.
fn read_user(file: Option<&Path>) -> Option<Value> {
    let file = file?;
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::info!(event = "coda_mcp_user_file_missing");
            return None;
        }
        Err(error) => {
            tracing::warn!(event = "coda_mcp_user_file_unreadable", kind = ?error.kind());
            return None;
        }
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    match serde_json::from_str::<Value>(text) {
        Ok(document)
            if document.is_object() && document.get("mcpServers").is_none_or(Value::is_object) =>
        {
            Some(document)
        }
        _ => {
            tracing::warn!(event = "coda_mcp_user_file_invalid");
            None
        }
    }
}

/// Writes `<link.dir>/.mcp.json` (atomically; on Unix readable by the user only, as it
/// holds the token) from the user's `user_file`; returns the folder for
/// `CODA_USER_MCP_DIR`.
pub fn write_mcp_config(link: &ToolsLink, user_file: Option<&Path>) -> io::Result<PathBuf> {
    let (document, replaced) = merged(read_user(user_file), link);
    if replaced {
        tracing::warn!(event = "coda_mcp_push_replaced");
    }
    create_private_dir(&link.dir)?;
    let path = link.dir.join(MCP_FILE);
    let temporary = link.dir.join(format!("{MCP_FILE}.tmp"));
    let json = serde_json::to_vec_pretty(&document).expect("the document serializes");
    let written = create_private_file(&temporary).and_then(|mut file| {
        file.write_all(&json)?;
        file.sync_all()
    });
    written
        .and_then(|()| std::fs::rename(&temporary, &path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })?;
    Ok(link.dir.clone())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    fn link(dir: &TestDir) -> ToolsLink {
        ToolsLink {
            endpoint: r"\\.\pipe\push-agent-bridge-0011-tools".into(),
            token: "secret-token".into(),
            exe: PathBuf::from("/opt/push/push-agent-bridge"),
            dir: dir.path().join(MCP_DIR),
        }
    }

    fn written(dir: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(dir.join(MCP_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn the_user_file_follows_coda_s_rules() {
        let home = Some(PathBuf::from("/home/me"));
        assert_eq!(
            user_mcp_file(None, home.clone()),
            Some(PathBuf::from("/home/me/.coda/.mcp.json"))
        );
        assert_eq!(
            user_mcp_file(Some("/etc/coda-mcp".into()), home.clone()),
            Some(PathBuf::from("/etc/coda-mcp/.mcp.json"))
        );
        assert_eq!(
            user_mcp_file(Some(OsString::new()), home),
            Some(PathBuf::from("/home/me/.coda/.mcp.json"))
        );
        assert_eq!(user_mcp_file(None, None), None);
    }

    #[test]
    fn the_user_s_servers_are_kept_and_push_is_set() {
        let dir = TestDir::new();
        let user = dir.mkdir("user").join(MCP_FILE);
        let servers = json!({
            "mcpServers": {
                "files": {"command": "files-mcp", "args": ["--root", "/x"], "env": {"A": "1"},
                          "disabled": true, "extra": [1, 2]},
                "web": {"type": "http", "url": "https://mcp.example/api",
                        "headers": {"X": "y"}},
                "push": {"command": "old-push"},
            },
            "other": {"kept": true},
        });
        std::fs::write(&user, serde_json::to_vec(&servers).unwrap()).unwrap();
        let link = link(&dir);
        let folder = write_mcp_config(&link, Some(&user)).unwrap();
        assert_eq!(folder, link.dir);
        let document = written(&folder);
        assert_eq!(
            document["mcpServers"]["files"],
            servers["mcpServers"]["files"]
        );
        assert_eq!(document["mcpServers"]["web"], servers["mcpServers"]["web"]);
        assert_eq!(document["other"], servers["other"]);
        assert_eq!(
            document["mcpServers"]["push"],
            json!({
                "command": "/opt/push/push-agent-bridge",
                "args": ["tools-mcp"],
                "env": {
                    "PUSH_BRIDGE_TOOLS": r"\\.\pipe\push-agent-bridge-0011-tools",
                    "PUSH_BRIDGE_TOOLS_TOKEN": "secret-token",
                },
            })
        );
        assert!(!folder.join(".mcp.json.tmp").exists());
    }

    #[test]
    fn a_user_file_with_a_bom_is_kept() {
        let dir = TestDir::new();
        let user = dir.path().join("bom.json");
        std::fs::write(
            &user,
            "\u{feff}{\"mcpServers\": {\"files\": {\"command\": \"files-mcp\"}}}",
        )
        .unwrap();
        let folder = write_mcp_config(&link(&dir), Some(&user)).unwrap();
        assert_eq!(
            written(&folder)["mcpServers"]["files"],
            json!({"command": "files-mcp"})
        );
    }

    #[test]
    fn without_a_usable_user_file_only_push_is_there() {
        let dir = TestDir::new();
        let link = link(&dir);
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        let shapeless = dir.path().join("shapeless.json");
        std::fs::write(&shapeless, r#"{"mcpServers": ["a"]}"#).unwrap();
        for user in [
            None,
            Some(dir.path().join("missing.json")),
            Some(broken),
            Some(shapeless),
        ] {
            let folder = write_mcp_config(&link, user.as_deref()).unwrap();
            let document = written(&folder);
            let servers = document["mcpServers"].as_object().unwrap();
            assert_eq!(servers.keys().collect::<Vec<_>>(), ["push"], "{user:?}");
        }
    }

    #[test]
    fn writing_again_replaces_the_file() {
        let dir = TestDir::new();
        let mut link = link(&dir);
        write_mcp_config(&link, None).unwrap();
        link.token = "next-token".into();
        let folder = write_mcp_config(&link, None).unwrap();
        assert_eq!(
            written(&folder)["mcpServers"]["push"]["env"]["PUSH_BRIDGE_TOOLS_TOKEN"],
            "next-token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_only_the_user_reads_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        let folder = write_mcp_config(&link(&dir), None).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&folder.join(MCP_FILE)), 0o600);
        assert_eq!(mode(&folder), 0o700);
    }

    #[test]
    fn debug_output_hides_the_token() {
        let dir = TestDir::new();
        assert!(!format!("{:?}", link(&dir)).contains("secret-token"));
    }
}
