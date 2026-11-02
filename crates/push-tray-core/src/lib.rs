//! The agent tray's core, free of any UI toolkit: agent states and their icons and
//! labels ([`state`], [`icon`]), the restart policy ([`policy`]), a state history
//! ([`history`]), the tray's `tray.json` ([`tray_config`]) and folders ([`paths`]), the
//! tray's own start at logon ([`autostart`]), the switch-over from Scheduled Tasks
//! ([`schedtask`]) and opening a file or folder ([`open`]).

pub mod autostart;
pub mod history;
pub mod icon;
pub mod open;
pub mod paths;
pub mod policy;
pub mod schedtask;
pub mod state;
pub mod tray_config;

#[cfg(test)]
pub(crate) mod test_dir;

/// The Windows `CREATE_NO_WINDOW` process creation flag: child processes get no console.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `%SystemRoot%\<relative>` (`C:\Windows` without `SystemRoot`), so that `PATH` does not
/// choose the program.
#[cfg(windows)]
pub(crate) fn system_program(relative: &str) -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot")
        .filter(|root| !root.is_empty())
        .map_or_else(
            || std::path::PathBuf::from(r"C:\Windows"),
            std::path::PathBuf::from,
        );
    root.join(relative)
}

/// A command for `program` that opens no console window on Windows.
pub(crate) fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
