//! Where the tray keeps `tray.json`, and the default bridge folder (the same rule as the
//! bridge's own default, `push_bridge::config::default_dir`).
//!
//! Each function has a `_from` variant taking the environment lookup, for tests.

use std::ffi::OsString;
use std::path::PathBuf;

/// `%LOCALAPPDATA%\push-messenger\tray.json` on Windows, else
/// `$XDG_CONFIG_HOME/push-messenger/tray.json` (default `~/.config/…`).
pub fn tray_json_path() -> PathBuf {
    tray_json_path_from(real_env)
}

/// [`tray_json_path`] with the environment given.
pub fn tray_json_path_from(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    #[cfg(windows)]
    let base = local_app_data(&env);
    #[cfg(not(windows))]
    let base = xdg_config_home(&env);
    base.join("push-messenger").join("tray.json")
}

/// The bridge's default folder: `%LOCALAPPDATA%\push-messenger\coda-bridge` on Windows,
/// else `$XDG_STATE_HOME/push-messenger/coda-bridge` (default `~/.local/state/…`).
pub fn default_bridge_dir() -> PathBuf {
    default_bridge_dir_from(real_env)
}

/// [`default_bridge_dir`] with the environment given.
pub fn default_bridge_dir_from(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    #[cfg(windows)]
    let base = local_app_data(&env);
    #[cfg(not(windows))]
    let base = xdg_state_home(&env);
    base.join("push-messenger").join("coda-bridge")
}

/// Where desktop sessions look for autostart entries (Linux):
/// `$XDG_CONFIG_HOME/autostart` (default `~/.config/autostart`).
pub fn xdg_autostart_dir() -> PathBuf {
    xdg_autostart_dir_from(real_env)
}

/// [`xdg_autostart_dir`] with the environment given.
pub fn xdg_autostart_dir_from(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    xdg_config_home(&env).join("autostart")
}

fn real_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

fn non_empty(env: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<PathBuf> {
    env(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `%LOCALAPPDATA%`, else `%USERPROFILE%\AppData\Local`, else `.`.
#[cfg_attr(not(windows), allow(dead_code))]
fn local_app_data(env: &impl Fn(&str) -> Option<OsString>) -> PathBuf {
    if let Some(local) = non_empty(env, "LOCALAPPDATA") {
        return local;
    }
    match non_empty(env, "USERPROFILE") {
        Some(profile) => profile.join("AppData").join("Local"),
        None => PathBuf::from("."),
    }
}

/// An absolute `$<name>`, else `$HOME/<fallback>`, else `.`.
fn xdg_home(env: &impl Fn(&str) -> Option<OsString>, name: &str, fallback: &[&str]) -> PathBuf {
    if let Some(dir) = non_empty(env, name).filter(|path| path.is_absolute()) {
        return dir;
    }
    match non_empty(env, "HOME") {
        Some(home) => fallback.iter().fold(home, |path, part| path.join(part)),
        None => PathBuf::from("."),
    }
}

fn xdg_config_home(env: &impl Fn(&str) -> Option<OsString>) -> PathBuf {
    xdg_home(env, "XDG_CONFIG_HOME", &[".config"])
}

#[cfg_attr(windows, allow(dead_code))]
fn xdg_state_home(env: &impl Fn(&str) -> Option<OsString>) -> PathBuf {
    xdg_home(env, "XDG_STATE_HOME", &[".local", "state"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    /// An absolute path on this platform.
    fn abs(unix: &str) -> String {
        if cfg!(windows) {
            format!(r"C:{}", unix.replace('/', "\\"))
        } else {
            unix.to_owned()
        }
    }

    #[test]
    fn local_app_data_falls_back_to_the_profile_then_here() {
        let both = env(&[("LOCALAPPDATA", "L"), ("USERPROFILE", "P")]);
        assert_eq!(local_app_data(&both), PathBuf::from("L"));
        let profile = env(&[("LOCALAPPDATA", ""), ("USERPROFILE", "P")]);
        assert_eq!(
            local_app_data(&profile),
            Path::new("P").join("AppData").join("Local")
        );
        assert_eq!(local_app_data(&env(&[])), PathBuf::from("."));
    }

    #[test]
    fn xdg_homes_need_an_absolute_value() {
        let config = abs("/cfg");
        let state = abs("/st");
        let set = env(&[
            ("XDG_CONFIG_HOME", &config),
            ("XDG_STATE_HOME", &state),
            ("HOME", "H"),
        ]);
        assert_eq!(xdg_config_home(&set), PathBuf::from(&config));
        assert_eq!(xdg_state_home(&set), PathBuf::from(&state));
        let relative = env(&[
            ("XDG_CONFIG_HOME", "rel"),
            ("XDG_STATE_HOME", ""),
            ("HOME", "H"),
        ]);
        assert_eq!(xdg_config_home(&relative), Path::new("H").join(".config"));
        assert_eq!(
            xdg_state_home(&relative),
            Path::new("H").join(".local").join("state")
        );
        assert_eq!(xdg_config_home(&env(&[])), PathBuf::from("."));
        assert_eq!(
            xdg_autostart_dir_from(&relative),
            Path::new("H").join(".config").join("autostart")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_live_under_local_app_data() {
        let vars = env(&[("LOCALAPPDATA", r"C:\Users\u\AppData\Local")]);
        assert_eq!(
            tray_json_path_from(&vars),
            PathBuf::from(r"C:\Users\u\AppData\Local\push-messenger\tray.json")
        );
        assert_eq!(
            default_bridge_dir_from(&vars),
            PathBuf::from(r"C:\Users\u\AppData\Local\push-messenger\coda-bridge")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_paths_follow_xdg() {
        let vars = env(&[("HOME", "/home/u")]);
        assert_eq!(
            tray_json_path_from(&vars),
            PathBuf::from("/home/u/.config/push-messenger/tray.json")
        );
        assert_eq!(
            default_bridge_dir_from(&vars),
            PathBuf::from("/home/u/.local/state/push-messenger/coda-bridge")
        );
        let xdg = env(&[("XDG_CONFIG_HOME", "/c"), ("XDG_STATE_HOME", "/s")]);
        assert_eq!(
            tray_json_path_from(&xdg),
            PathBuf::from("/c/push-messenger/tray.json")
        );
        assert_eq!(
            default_bridge_dir_from(&xdg),
            PathBuf::from("/s/push-messenger/coda-bridge")
        );
    }
}
