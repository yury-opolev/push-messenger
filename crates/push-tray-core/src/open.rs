//! Opens a file or folder with the desktop's default program.

use std::io;
use std::path::Path;
use std::process::Stdio;

/// Opens `path` (absolute) with `explorer.exe` on Windows, `xdg-open` elsewhere: the path is
/// one argument, no shell. Does not wait for the program.
pub fn open_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not an absolute path: {}", path.display()),
        ));
    }
    let mut child = opener()
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap it in the background; explorer.exe's exit code means nothing.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn opener() -> std::process::Command {
    crate::hidden_command(crate::system_program("explorer.exe"))
}

#[cfg(not(windows))]
fn opener() -> std::process::Command {
    crate::hidden_command("xdg-open")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_refused() {
        let error = open_path(Path::new("logs")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = open_path(Path::new("-n")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
