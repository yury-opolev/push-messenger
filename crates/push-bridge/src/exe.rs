//! The agent's program file (Coda's executable): where it is, whether it can be used and
//! a fingerprint that changes when it is replaced, for upgrade detection.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(windows)]
pub mod windows;

/// What identifies one build of a program file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
    /// The Windows file version, such as `0.1.176`; `None` elsewhere or without one.
    pub version: Option<String>,
}

/// The state of a program file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Not there (or not found in `PATH`).
    Missing,
    /// There but not readable yet: held by an installer, or still empty.
    Locked,
    Usable(Fingerprint),
}

/// `exe` as configured: a path with a separator is used as it is; a bare name is looked
/// up in `PATH` (Windows: with each `PATHEXT` extension when it has none). `None`: not
/// found.
pub fn resolve(exe: &Path) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH");
    let pathext = std::env::var_os("PATHEXT");
    resolve_in(exe, path_var.as_deref(), pathext.as_deref())
}

/// [`resolve`] with `PATH` and `PATHEXT` given.
pub fn resolve_in(
    exe: &Path,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Option<PathBuf> {
    let has_separator = exe
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());
    if has_separator {
        return Some(exe.to_owned());
    }
    if exe.as_os_str().is_empty() {
        return None;
    }
    std::env::split_paths(path_var?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates(&dir.join(exe), pathext))
        .find(|candidate| candidate.is_file())
}

/// The files `path` (a folder joined with a bare name) may be.
#[cfg(windows)]
fn candidates(path: &Path, pathext: Option<&OsStr>) -> Vec<PathBuf> {
    /// What `cmd.exe` uses when `PATHEXT` is not set.
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
    if path.extension().is_some() {
        return vec![path.to_owned()];
    }
    let pathext = pathext.and_then(OsStr::to_str).unwrap_or(DEFAULT_PATHEXT);
    pathext
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .map(|extension| {
            let mut name = path.as_os_str().to_owned();
            name.push(extension);
            PathBuf::from(name)
        })
        .collect()
}

/// The files `path` (a folder joined with a bare name) may be.
#[cfg(not(windows))]
fn candidates(path: &Path, _pathext: Option<&OsStr>) -> Vec<PathBuf> {
    vec![path.to_owned()]
}

/// Missing: [`resolve`] fails or the file is not there; Locked: opening it for reading
/// fails otherwise (on Windows a sharing violation, a lock or access denied) or it is
/// empty; Usable: opened, with its fingerprint.
pub fn probe(exe: &Path) -> Probe {
    match resolve(exe) {
        Some(path) => probe_path(path),
        None => Probe::Missing,
    }
}

fn probe_path(path: PathBuf) -> Probe {
    if path.is_dir() {
        return Probe::Missing;
    }
    let metadata = match File::open(&path).and_then(|file| file.metadata()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Probe::Missing,
        Err(_) => return Probe::Locked,
    };
    if metadata.len() == 0 {
        return Probe::Locked;
    }
    let version = file_version(&path);
    Probe::Usable(Fingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        version,
        path,
    })
}

#[cfg(windows)]
fn file_version(path: &Path) -> Option<String> {
    windows::file_version(path)
}

#[cfg(not(windows))]
fn file_version(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::test_dir::TestDir;

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn a_path_with_a_separator_is_used_as_it_is() {
        let exe = Path::new("tools").join("coda");
        assert_eq!(resolve_in(&exe, None, None), Some(exe.clone()));
        let path = path_of(&[Path::new("elsewhere")]);
        assert_eq!(resolve_in(&exe, Some(&path), None), Some(exe));
    }

    #[test]
    fn a_bare_name_is_found_in_path() {
        let dir = TestDir::new();
        let empty = dir.mkdir("empty");
        let bin = dir.mkdir("bin");
        let file = if cfg!(windows) { "tool.EXE" } else { "tool" };
        std::fs::write(bin.join(file), b"x").unwrap();
        let path = path_of(&[&empty, &bin]);
        let pathext = OsString::from(".COM;.EXE");
        assert_eq!(
            resolve_in(Path::new("tool"), Some(&path), Some(&pathext)),
            Some(bin.join(file))
        );
        assert_eq!(
            resolve_in(Path::new(file), Some(&path), Some(&pathext)),
            Some(bin.join(file))
        );
        assert_eq!(
            resolve_in(Path::new("other"), Some(&path), Some(&pathext)),
            None
        );
        assert_eq!(resolve_in(Path::new("tool"), None, Some(&pathext)), None);
    }

    #[test]
    fn a_missing_program_is_missing() {
        let dir = TestDir::new();
        assert_eq!(probe(&dir.path().join("coda.exe")), Probe::Missing);
        assert_eq!(
            probe(Path::new("push-bridge-test-no-such-program")),
            Probe::Missing
        );
        assert_eq!(probe(dir.path()), Probe::Missing);
    }

    #[test]
    fn a_written_program_is_usable_and_its_fingerprint_follows_the_file() {
        let dir = TestDir::new();
        let exe = dir.path().join("coda.exe");
        std::fs::write(&exe, b"build one").unwrap();
        let Probe::Usable(first) = probe(&exe) else {
            panic!("usable");
        };
        assert_eq!(first.path, exe);
        assert_eq!(first.len, 9);
        assert!(first.modified.is_some());
        assert_eq!(probe(&exe), Probe::Usable(first.clone()));
        std::fs::write(&exe, b"build number two").unwrap();
        let Probe::Usable(second) = probe(&exe) else {
            panic!("usable");
        };
        assert_eq!(second.len, 16);
        assert_ne!(second, first);
    }

    #[test]
    fn an_empty_program_is_locked() {
        let dir = TestDir::new();
        let exe = dir.path().join("coda.exe");
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(probe(&exe), Probe::Locked);
    }

    #[cfg(windows)]
    #[test]
    fn a_program_another_handle_holds_is_locked() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = TestDir::new();
        let exe = dir.path().join("coda.exe");
        std::fs::write(&exe, b"build").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&exe)
            .unwrap();
        assert_eq!(probe(&exe), Probe::Locked);
        drop(held);
        assert!(matches!(probe(&exe), Probe::Usable(_)));
    }
}
