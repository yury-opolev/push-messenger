//! The tray's own start at logon: the `HKCU\…\Run` value on Windows, an XDG autostart
//! entry elsewhere.

use std::io;
use std::path::{Path, PathBuf};

/// The tray's start at logon.
pub trait Autostart {
    fn enabled(&self) -> io::Result<bool>;
    fn set(&self, on: bool) -> io::Result<()>;
}

/// The value's name under the Run key, and the desktop entry's name.
pub const APP_NAME: &str = "push-agent-tray";
/// The argument the tray gets when started at logon.
pub const AUTOSTART_ARG: &str = "--autostart";
/// The current user's Run key.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// The Run value's data: `"<exe>" --autostart`.
pub fn run_key_value(exe: &Path) -> String {
    format!("\"{}\" {AUTOSTART_ARG}", exe.display())
}

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\push-agent-tray`, through `reg.exe`.
#[derive(Debug, Clone)]
pub struct RunKey {
    pub exe: PathBuf,
}

impl Autostart for RunKey {
    fn enabled(&self) -> io::Result<bool> {
        let status = reg(&["query", RUN_KEY, "/v", APP_NAME])?;
        Ok(status.success())
    }

    fn set(&self, on: bool) -> io::Result<()> {
        let status = if on {
            let value = run_key_value(&self.exe);
            reg(&[
                "add", RUN_KEY, "/v", APP_NAME, "/t", "REG_SZ", "/d", &value, "/f",
            ])?
        } else {
            if !self.enabled()? {
                return Ok(());
            }
            reg(&["delete", RUN_KEY, "/v", APP_NAME, "/f"])?
        };
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("reg.exe failed ({status})")))
        }
    }
}

#[cfg(windows)]
fn reg(args: &[&str]) -> io::Result<std::process::ExitStatus> {
    use std::process::Stdio;
    crate::hidden_command(crate::system_program(r"System32\reg.exe"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(not(windows))]
fn reg(_args: &[&str]) -> io::Result<std::process::ExitStatus> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the Run key exists only on Windows",
    ))
}

/// `<dir>/push-agent-tray.desktop`, where `dir` is the XDG autostart folder
/// ([`crate::paths::xdg_autostart_dir`]).
#[derive(Debug, Clone)]
pub struct XdgAutostart {
    pub dir: PathBuf,
    pub exe: PathBuf,
}

impl XdgAutostart {
    /// The desktop entry's path.
    pub fn file(&self) -> PathBuf {
        self.dir.join(format!("{APP_NAME}.desktop"))
    }
}

/// The desktop entry that starts `exe --autostart`.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={APP_NAME}\nExec={} {AUTOSTART_ARG}\nX-GNOME-Autostart-enabled=true\n",
        desktop_exec_quote(&exe.to_string_lossy())
    )
}

/// Quotes a program path for a desktop entry's `Exec` key: in double quotes, with `"`,
/// `` ` ``, `$` and `\` escaped by a backslash, and `%` doubled (a field code otherwise).
fn desktop_exec_quote(text: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '%' => quoted.push_str("%%"),
            '\n' | '\r' => quoted.push(' '),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

impl Autostart for XdgAutostart {
    fn enabled(&self) -> io::Result<bool> {
        match std::fs::metadata(self.file()) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn set(&self, on: bool) -> io::Result<()> {
        let file = self.file();
        if on {
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(&file, desktop_entry(&self.exe))
        } else {
            match std::fs::remove_file(&file) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    #[test]
    fn the_run_value_quotes_the_exe() {
        assert_eq!(
            run_key_value(Path::new(
                r"C:\Program Files\push-agent-tray\push-agent-tray.exe"
            )),
            r#""C:\Program Files\push-agent-tray\push-agent-tray.exe" --autostart"#
        );
    }

    #[test]
    fn the_desktop_entry_runs_the_exe_with_autostart() {
        assert_eq!(
            desktop_entry(Path::new("/opt/push agent/push-agent-tray")),
            "[Desktop Entry]\nType=Application\nName=push-agent-tray\nExec=\"/opt/push agent/push-agent-tray\" --autostart\nX-GNOME-Autostart-enabled=true\n"
        );
        assert_eq!(
            desktop_exec_quote(r#"/a"b$c`d\e%f"#),
            r#""/a\"b\$c\`d\\e%%f""#
        );
    }

    #[test]
    fn xdg_autostart_writes_and_removes_its_entry() {
        let temp = TestDir::new();
        let autostart = XdgAutostart {
            dir: temp.path().join("config").join("autostart"),
            exe: PathBuf::from("/usr/bin/push-agent-tray"),
        };
        assert!(!autostart.enabled().unwrap());
        autostart.set(false).unwrap();
        autostart.set(true).unwrap();
        assert!(autostart.enabled().unwrap());
        let text = std::fs::read_to_string(autostart.file()).unwrap();
        assert!(text.contains("Exec=\"/usr/bin/push-agent-tray\" --autostart\n"));
        autostart.set(true).unwrap();
        autostart.set(false).unwrap();
        assert!(!autostart.enabled().unwrap());
        assert!(!autostart.file().exists());
    }
}
