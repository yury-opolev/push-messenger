//! Which endpoint a bridge listens on: an instance id derived from its folder, and the
//! named pipe (Windows) or Unix socket for that id.

use std::ffi::OsStr;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use sha2::{Digest, Sha256};

/// 16 lowercase hex chars: the first 8 bytes of SHA-256 over the normalized folder.
///
/// Normalized = `std::fs::canonicalize` (else `std::path::absolute`, else as given) as a
/// string; on Windows without a leading `\\?\` (and `\\?\UNC\` → `\\`), with `\` and in
/// lowercase; on Unix unchanged in case; without a trailing separator.
pub fn instance_id(dir: &Path) -> String {
    let digest = Sha256::digest(normalize(dir).as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize(dir: &Path) -> String {
    let full = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf());
    let mut text = full.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        text = text.replace('/', "\\");
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            text = format!(r"\\{rest}");
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            text = rest.to_owned();
        }
        text = text.to_lowercase();
    }
    while text.len() > 1 && text.ends_with(MAIN_SEPARATOR) {
        text.pop();
    }
    text
}

/// Where a bridge's control channel listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `\\.\pipe\push-agent-bridge-<id>`
    Pipe(String),
    /// `$XDG_RUNTIME_DIR/push-agent-bridge/<id>.sock`
    Socket(PathBuf),
}

/// The endpoint for an instance id. Windows: always a pipe. Unix: `None` when
/// `XDG_RUNTIME_DIR` is unset, empty or relative (the control channel is then off).
pub fn endpoint(id: &str) -> Option<Endpoint> {
    #[cfg(windows)]
    {
        endpoint_for(id, None)
    }
    #[cfg(not(windows))]
    {
        endpoint_for(id, std::env::var_os("XDG_RUNTIME_DIR").as_deref())
    }
}

/// [`endpoint`] with the runtime dir (the value of `XDG_RUNTIME_DIR`) given; Windows
/// ignores it.
pub fn endpoint_for(id: &str, runtime_dir: Option<&OsStr>) -> Option<Endpoint> {
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        Some(Endpoint::Pipe(format!(r"\\.\pipe\push-agent-bridge-{id}")))
    }
    #[cfg(not(windows))]
    {
        let dir = Path::new(runtime_dir?);
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            return None;
        }
        Some(Endpoint::Socket(
            dir.join("push-agent-bridge").join(format!("{id}.sock")),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_id(id: &str) -> bool {
        id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn the_id_is_stable_and_16_hex() {
        let here = std::env::current_dir().unwrap();
        let id = instance_id(&here);
        assert!(is_id(&id), "{id}");
        assert_eq!(instance_id(&here), id);
    }

    #[test]
    fn two_folders_have_different_ids() {
        let here = std::env::current_dir().unwrap();
        assert_ne!(instance_id(&here), instance_id(&here.join("src")));
        assert_ne!(
            instance_id(Path::new("no-such-folder-a")),
            instance_id(Path::new("no-such-folder-b"))
        );
    }

    #[test]
    fn a_trailing_separator_does_not_matter() {
        let missing = std::env::current_dir().unwrap().join("no-such-folder-pbc");
        let mut with_separator = missing.clone().into_os_string();
        with_separator.push(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(
            instance_id(Path::new(&with_separator)),
            instance_id(&missing)
        );
        let here = std::env::current_dir().unwrap();
        let mut here_separator = here.clone().into_os_string();
        here_separator.push(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(instance_id(Path::new(&here_separator)), instance_id(&here));
    }

    #[cfg(windows)]
    #[test]
    fn windows_ids_ignore_case_and_slashes() {
        assert_eq!(
            instance_id(Path::new(r"C:\No-Such\Folder-PBC")),
            instance_id(Path::new(r"c:\no-such\folder-pbc"))
        );
        assert_eq!(
            instance_id(Path::new("C:/no-such/folder-pbc")),
            instance_id(Path::new(r"c:\no-such\folder-pbc"))
        );
        let here = std::env::current_dir().unwrap();
        let upper = here.to_string_lossy().to_uppercase();
        assert_eq!(instance_id(Path::new(&upper)), instance_id(&here));
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoints_are_pipes() {
        let id = "00112233aabbccdd";
        let pipe = Endpoint::Pipe(r"\\.\pipe\push-agent-bridge-00112233aabbccdd".into());
        assert_eq!(endpoint_for(id, None), Some(pipe.clone()));
        assert_eq!(
            endpoint_for(id, Some(OsStr::new("rel"))),
            Some(pipe.clone())
        );
        assert_eq!(endpoint(id), Some(pipe));
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoints_are_sockets_under_the_runtime_dir() {
        let id = "00112233aabbccdd";
        assert_eq!(
            endpoint_for(id, Some(OsStr::new("/run/user/1000"))),
            Some(Endpoint::Socket(PathBuf::from(
                "/run/user/1000/push-agent-bridge/00112233aabbccdd.sock"
            )))
        );
        assert_eq!(endpoint_for(id, None), None);
        assert_eq!(endpoint_for(id, Some(OsStr::new(""))), None);
        assert_eq!(endpoint_for(id, Some(OsStr::new("run/user"))), None);
    }
}
