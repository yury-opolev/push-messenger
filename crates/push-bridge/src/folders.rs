//! The working folder: `/cd` accepts only a real path (symlinks and `..` resolved) inside
//! one of the configured roots.

use std::path::{Path, PathBuf};

/// Why a folder was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FolderError {
    #[error("no such folder")]
    NotFound,
    #[error("not a folder")]
    NotADirectory,
    #[error("outside the allowed roots")]
    OutsideRoots,
}

/// The real path of `requested` (symlinks, junctions and `..` resolved by
/// [`std::fs::canonicalize`]) when it is a directory equal to or inside one of `roots`
/// (each canonicalized too; roots that do not exist are skipped). Relative paths resolve
/// against `current`. On Windows the result has no `\\?\` prefix (`\\?\UNC\server\…`
/// becomes `\\server\…`).
pub fn resolve(requested: &str, current: &Path, roots: &[PathBuf]) -> Result<PathBuf, FolderError> {
    let real = std::fs::canonicalize(current.join(requested)).map_err(|_| FolderError::NotFound)?;
    // Component-wise: `C:\src2` is not inside `C:\src`.
    let inside = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| real.starts_with(root));
    if !inside {
        return Err(FolderError::OutsideRoots);
    }
    if !real.is_dir() {
        return Err(FolderError::NotADirectory);
    }
    Ok(simplified(real))
}

/// The path without the verbatim prefix `canonicalize` adds on Windows.
#[cfg(windows)]
fn simplified(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

#[cfg(not(windows))]
fn simplified(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    struct Fixture {
        dir: TestDir,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = TestDir::new();
        let root = dir.mkdir("src");
        dir.mkdir("src/app/inner");
        dir.mkdir("src2");
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        Fixture { dir, root }
    }

    fn real(path: &Path) -> PathBuf {
        simplified(std::fs::canonicalize(path).unwrap())
    }

    fn roots(fixture: &Fixture) -> Vec<PathBuf> {
        vec![
            fixture.dir.path().join("missing-root"),
            fixture.root.clone(),
        ]
    }

    #[test]
    fn a_folder_inside_a_root_is_accepted() {
        let fixture = fixture();
        let roots = roots(&fixture);
        let inner = real(&fixture.root.join("app").join("inner"));
        assert_eq!(
            resolve("app/inner", &fixture.root, &roots),
            Ok(inner.clone())
        );
        let absolute = inner.to_str().unwrap();
        assert_eq!(
            resolve(absolute, Path::new("/elsewhere"), &roots),
            Ok(inner.clone())
        );
        assert_eq!(
            resolve("inner/../inner", &fixture.root.join("app"), &roots),
            Ok(inner)
        );
    }

    #[test]
    fn the_root_itself_is_accepted() {
        let fixture = fixture();
        let roots = roots(&fixture);
        assert_eq!(resolve(".", &fixture.root, &roots), Ok(real(&fixture.root)));
    }

    #[cfg(windows)]
    #[test]
    fn the_result_has_no_verbatim_prefix() {
        let fixture = fixture();
        let resolved = resolve(".", &fixture.root, &roots(&fixture)).unwrap();
        assert!(!resolved.to_str().unwrap().starts_with(r"\\?\"));
        assert_eq!(
            simplified(PathBuf::from(r"\\?\UNC\server\share\x")),
            PathBuf::from(r"\\server\share\x")
        );
    }

    #[test]
    fn a_dot_dot_escape_is_refused() {
        let fixture = fixture();
        let roots = roots(&fixture);
        assert_eq!(
            resolve("..", &fixture.root, &roots),
            Err(FolderError::OutsideRoots)
        );
        assert_eq!(
            resolve("app/../../src2", &fixture.root, &roots),
            Err(FolderError::OutsideRoots)
        );
    }

    #[test]
    fn a_sibling_sharing_the_roots_prefix_is_refused() {
        let fixture = fixture();
        let sibling = fixture.dir.path().join("src2");
        assert_eq!(
            resolve(sibling.to_str().unwrap(), &fixture.root, &roots(&fixture)),
            Err(FolderError::OutsideRoots)
        );
    }

    #[test]
    fn a_file_is_refused() {
        let fixture = fixture();
        assert_eq!(
            resolve("notes.txt", &fixture.root, &roots(&fixture)),
            Err(FolderError::NotADirectory)
        );
    }

    #[test]
    fn a_missing_folder_is_refused() {
        let fixture = fixture();
        assert_eq!(
            resolve("nope", &fixture.root, &roots(&fixture)),
            Err(FolderError::NotFound)
        );
    }

    #[test]
    fn no_roots_refuse_everything() {
        let fixture = fixture();
        assert_eq!(
            resolve(".", &fixture.root, &[]),
            Err(FolderError::OutsideRoots)
        );
    }

    #[test]
    fn a_symlink_out_of_the_root_is_refused() {
        let fixture = fixture();
        let outside = fixture.dir.mkdir("outside");
        let link = fixture.root.join("escape");
        if !link_dir(&outside, &link) {
            eprintln!("note: this host allows neither directory symlinks nor junctions; skipped");
            return;
        }
        let roots = roots(&fixture);
        assert_eq!(
            resolve("escape", &fixture.root, &roots),
            Err(FolderError::OutsideRoots)
        );
        // A link that stays inside the roots is followed.
        let inner_link = fixture.root.join("shortcut");
        if link_dir(&fixture.root.join("app"), &inner_link) {
            assert_eq!(
                resolve("shortcut", &fixture.root, &roots),
                Ok(real(&fixture.root.join("app")))
            );
        }
    }

    /// A directory link at `link` to `target`: a symlink, or on Windows without the
    /// privilege for one, a junction (which needs none).
    #[cfg(windows)]
    fn link_dir(target: &Path, link: &Path) -> bool {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return true;
        }
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
            && link.exists()
    }

    #[cfg(unix)]
    fn link_dir(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}
