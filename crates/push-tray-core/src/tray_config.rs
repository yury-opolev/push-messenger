//! `tray.json`: the bridge folders the tray supervises.
//!
//! ```json
//! { "version": 1, "instances": [ { "dir": "C:\\…\\coda-bridge", "autostart": true } ] }
//! ```

use std::path::{Path, PathBuf};

use push_bridge_control::instance_id;
use serde::{Deserialize, Serialize};

/// The `version` this tray writes and reads.
pub const VERSION: u32 = 1;

/// Why `tray.json` could not be read or written, or a folder not added.
#[derive(Debug, thiserror::Error)]
pub enum TrayConfigError {
    #[error("cannot read or write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not valid: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{} has version {version}; this tray reads version {VERSION}", path.display())]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("{} is not a bridge folder: it has no config.json", .0.display())]
    NotABridgeFolder(PathBuf),
}

/// The fewest chat peek messages the window keeps.
pub const PEEK_MESSAGES_MIN: u32 = 10;
/// The most chat peek messages the window keeps.
pub const PEEK_MESSAGES_MAX: u32 = 50;
/// The chat peek messages the window keeps unless set.
pub const PEEK_MESSAGES_DEFAULT: u32 = 20;

/// The contents of `tray.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrayConfig {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub instances: Vec<Instance>,
    /// Show each agent's last line in the tray menu.
    #[serde(default, skip_serializing_if = "is_false")]
    pub menu_peek: bool,
    /// No animated icons or badges.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reduce_motion: bool,
    /// Where the tray window was last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowPlace>,
}

/// The tray window's position and size, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowPlace {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// A bridge folder the tray supervises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub dir: PathBuf,
    /// Start it with the tray.
    #[serde(default = "yes")]
    pub autostart: bool,
    /// Show the agent's chat peek in the window.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub show_peek: bool,
    /// How many chat peek messages the window keeps; see [`Instance::peek_messages`].
    #[serde(
        default = "default_peek_messages",
        skip_serializing_if = "is_default_peek_messages"
    )]
    pub peek_messages: u32,
}

impl Instance {
    /// An instance for `dir` with the defaults: autostart on, peek shown, 20 messages.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            autostart: true,
            show_peek: true,
            peek_messages: PEEK_MESSAGES_DEFAULT,
        }
    }

    /// The chat peek messages to keep: `peek_messages` clamped to
    /// [`PEEK_MESSAGES_MIN`]`..=`[`PEEK_MESSAGES_MAX`].
    pub fn peek_messages(&self) -> u32 {
        clamp_peek_messages(self.peek_messages)
    }
}

/// `value` clamped to [`PEEK_MESSAGES_MIN`]`..=`[`PEEK_MESSAGES_MAX`].
pub fn clamp_peek_messages(value: u32) -> u32 {
    value.clamp(PEEK_MESSAGES_MIN, PEEK_MESSAGES_MAX)
}

fn one() -> u32 {
    VERSION
}

fn yes() -> bool {
    true
}

fn default_peek_messages() -> u32 {
    PEEK_MESSAGES_DEFAULT
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_peek_messages(value: &u32) -> bool {
    *value == PEEK_MESSAGES_DEFAULT
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            version: VERSION,
            instances: Vec::new(),
            menu_peek: false,
            reduce_motion: false,
            window: None,
        }
    }
}

impl TrayConfig {
    /// Reads `path`; a missing file is an empty configuration. A file that does not parse
    /// is an error: the tray shows it and never overwrites the file silently.
    pub fn load(path: &Path) -> Result<Self, TrayConfigError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(TrayConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        let config: Self =
            serde_json::from_slice(text).map_err(|source| TrayConfigError::Invalid {
                path: path.to_path_buf(),
                source,
            })?;
        if config.version != VERSION {
            return Err(TrayConfigError::UnsupportedVersion {
                path: path.to_path_buf(),
                version: config.version,
            });
        }
        Ok(config)
    }

    /// Writes `path` atomically: its folder is created, the JSON goes to `<path>.tmp`, which
    /// then replaces `path`.
    pub fn save(&self, path: &Path) -> Result<(), TrayConfigError> {
        let io = |source| TrayConfigError::Io {
            path: path.to_path_buf(),
            source,
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let mut text = serde_json::to_string_pretty(self).expect("tray.json serializes");
        text.push('\n');
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let written =
            write_synced(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(source) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(source));
        }
        Ok(())
    }

    /// The instance for `dir` (same instance id), if any.
    pub fn find(&self, dir: &Path) -> Option<&Instance> {
        let id = instance_id(dir);
        self.instances
            .iter()
            .find(|instance| instance_id(&instance.dir) == id)
    }

    fn find_mut(&mut self, dir: &Path) -> Option<&mut Instance> {
        let id = instance_id(dir);
        self.instances
            .iter_mut()
            .find(|instance| instance_id(&instance.dir) == id)
    }

    /// Adds the bridge folder `dir` (it must hold `config.json`) with autostart on; false
    /// when it is already there (the same instance id).
    pub fn add(&mut self, dir: &Path) -> Result<bool, TrayConfigError> {
        if !dir.join("config.json").is_file() {
            return Err(TrayConfigError::NotABridgeFolder(dir.to_path_buf()));
        }
        if self.find(dir).is_some() {
            return Ok(false);
        }
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        self.instances.push(Instance::new(dir));
        Ok(true)
    }

    /// Removes the instance for `dir`; false when there is none.
    pub fn remove(&mut self, dir: &Path) -> bool {
        let id = instance_id(dir);
        let before = self.instances.len();
        self.instances
            .retain(|instance| instance_id(&instance.dir) != id);
        self.instances.len() != before
    }

    /// Adds the bridge's default folder (autostart on) when it holds `config.json` and is
    /// not there yet; true when added.
    pub fn ensure_default(&mut self, default_dir: &Path) -> bool {
        matches!(self.add(default_dir), Ok(true))
    }

    /// Sets the instance's autostart; false when there is no instance for `dir`.
    pub fn set_autostart(&mut self, dir: &Path, on: bool) -> bool {
        match self.find_mut(dir) {
            Some(instance) => {
                instance.autostart = on;
                true
            }
            None => false,
        }
    }

    /// Sets whether the window shows the instance's chat peek; false when there is no
    /// instance for `dir`.
    pub fn set_show_peek(&mut self, dir: &Path, on: bool) -> bool {
        match self.find_mut(dir) {
            Some(instance) => {
                instance.show_peek = on;
                true
            }
            None => false,
        }
    }

    /// Sets how many chat peek messages the window keeps, clamped to
    /// [`PEEK_MESSAGES_MIN`]`..=`[`PEEK_MESSAGES_MAX`]; false when there is no instance
    /// for `dir`.
    pub fn set_peek_messages(&mut self, dir: &Path, messages: u32) -> bool {
        match self.find_mut(dir) {
            Some(instance) => {
                instance.peek_messages = clamp_peek_messages(messages);
                true
            }
            None => false,
        }
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    #[test]
    fn a_saved_configuration_loads_back() {
        let temp = TestDir::new();
        let path = temp.path().join("nested").join("tray.json");
        let config = TrayConfig {
            instances: vec![
                Instance::new(temp.path().join("a")),
                Instance {
                    autostart: false,
                    ..Instance::new(temp.path().join("b"))
                },
            ],
            ..TrayConfig::default()
        };
        config.save(&path).unwrap();
        assert_eq!(TrayConfig::load(&path).unwrap(), config);
        config.save(&path).unwrap();
        assert_eq!(TrayConfig::load(&path).unwrap(), config);
    }

    #[test]
    fn a_missing_file_is_empty() {
        let temp = TestDir::new();
        let config = TrayConfig::load(&temp.path().join("tray.json")).unwrap();
        assert_eq!(config, TrayConfig::default());
        assert_eq!(config.version, 1);
        assert!(config.instances.is_empty());
    }

    #[test]
    fn a_bom_and_defaults_are_fine() {
        let temp = TestDir::new();
        let path = temp.path().join("tray.json");
        std::fs::write(&path, b"\xef\xbb\xbf{\"instances\":[{\"dir\":\"x\"}]}").unwrap();
        let config = TrayConfig::load(&path).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.instances, [Instance::new(PathBuf::from("x"))]);
    }

    #[test]
    fn unknown_fields_corrupt_files_and_other_versions_are_refused() {
        let temp = TestDir::new();
        let path = temp.path().join("tray.json");
        for (text, invalid) in [
            (r#"{"version":1,"extra":true}"#, true),
            (r#"{"instances":[{"dir":"x","colour":"red"}]}"#, true),
            ("{not json", true),
            ("", true),
            (r#"{"version":2}"#, false),
        ] {
            std::fs::write(&path, text).unwrap();
            let error = TrayConfig::load(&path).unwrap_err();
            if invalid {
                assert!(matches!(error, TrayConfigError::Invalid { .. }), "{text}");
            } else {
                assert!(
                    matches!(
                        error,
                        TrayConfigError::UnsupportedVersion { version: 2, .. }
                    ),
                    "{text}"
                );
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn add_requires_config_json() {
        let temp = TestDir::new();
        let empty = temp.mkdir("empty");
        let mut config = TrayConfig::default();
        assert!(matches!(
            config.add(&empty),
            Err(TrayConfigError::NotABridgeFolder(_))
        ));
        assert!(config.instances.is_empty());
        let bridge = temp.bridge("bridge");
        assert!(config.add(&bridge).unwrap());
        assert_eq!(config.instances.len(), 1);
        assert!(config.instances[0].autostart);
        assert!(config.instances[0].dir.is_absolute());
    }

    #[test]
    fn add_dedupes_by_instance_id() {
        let temp = TestDir::new();
        let bridge = temp.bridge("Bridge");
        let mut config = TrayConfig::default();
        assert!(config.add(&bridge).unwrap());
        let mut trailing = bridge.clone().into_os_string();
        trailing.push(std::path::MAIN_SEPARATOR_STR);
        assert!(!config.add(Path::new(&trailing)).unwrap());
        #[cfg(windows)]
        {
            let upper = PathBuf::from(bridge.to_string_lossy().to_uppercase());
            assert!(!config.add(&upper).unwrap());
        }
        assert_eq!(config.instances.len(), 1);
        assert!(config.find(Path::new(&trailing)).is_some());
    }

    #[test]
    fn remove_and_set_autostart_find_the_instance() {
        let temp = TestDir::new();
        let a = temp.bridge("a");
        let b = temp.bridge("b");
        let mut config = TrayConfig::default();
        config.add(&a).unwrap();
        config.add(&b).unwrap();
        assert!(config.set_autostart(&a, false));
        assert!(!config.find(&a).unwrap().autostart);
        assert!(!config.set_autostart(&temp.path().join("c"), true));
        assert!(config.remove(&a));
        assert!(!config.remove(&a));
        assert_eq!(config.instances.len(), 1);
        assert_eq!(instance_id(&config.instances[0].dir), instance_id(&b));
    }

    #[test]
    fn ensure_default_adds_once_and_only_with_config_json() {
        let temp = TestDir::new();
        let default_dir = temp.mkdir("coda-bridge");
        let mut config = TrayConfig::default();
        assert!(!config.ensure_default(&default_dir));
        assert!(!config.ensure_default(&temp.path().join("missing")));
        std::fs::write(default_dir.join("config.json"), "{}").unwrap();
        assert!(config.ensure_default(&default_dir));
        assert!(!config.ensure_default(&default_dir));
        assert_eq!(config.instances.len(), 1);
        assert!(config.instances[0].autostart);
    }

    #[test]
    fn the_window_settings_round_trip() {
        let temp = TestDir::new();
        let path = temp.path().join("tray.json");
        let config = TrayConfig {
            instances: vec![Instance {
                show_peek: false,
                peek_messages: 35,
                ..Instance::new(temp.path().join("a"))
            }],
            menu_peek: true,
            reduce_motion: true,
            window: Some(WindowPlace {
                x: -10,
                y: 20,
                width: 900,
                height: 600,
            }),
            ..TrayConfig::default()
        };
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for key in [
            "\"show_peek\": false",
            "\"peek_messages\": 35",
            "\"menu_peek\": true",
            "\"reduce_motion\": true",
            "\"window\"",
        ] {
            assert!(text.contains(key), "{key} in {text}");
        }
        assert_eq!(TrayConfig::load(&path).unwrap(), config);
    }

    #[test]
    fn the_window_settings_default_and_are_not_written_when_default() {
        let temp = TestDir::new();
        let path = temp.path().join("tray.json");
        let old = "{\n  \"version\": 1,\n  \"instances\": [\n    {\n      \"dir\": \"x\",\n      \"autostart\": false\n    }\n  ]\n}\n";
        std::fs::write(&path, old).unwrap();
        let config = TrayConfig::load(&path).unwrap();
        assert!(!config.menu_peek);
        assert!(!config.reduce_motion);
        assert_eq!(config.window, None);
        assert!(config.instances[0].show_peek);
        assert_eq!(config.instances[0].peek_messages, PEEK_MESSAGES_DEFAULT);
        config.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        TrayConfig::default().save(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"version\": 1,\n  \"instances\": []\n}\n"
        );
    }

    #[test]
    fn peek_messages_out_of_range_load_and_are_clamped_on_use() {
        let temp = TestDir::new();
        let path = temp.path().join("tray.json");
        std::fs::write(
            &path,
            r#"{"instances":[{"dir":"a","peek_messages":3},{"dir":"b","peek_messages":500}]}"#,
        )
        .unwrap();
        let config = TrayConfig::load(&path).unwrap();
        assert_eq!(config.instances[0].peek_messages, 3);
        assert_eq!(config.instances[0].peek_messages(), PEEK_MESSAGES_MIN);
        assert_eq!(config.instances[1].peek_messages(), PEEK_MESSAGES_MAX);
        assert_eq!(clamp_peek_messages(30), 30);
    }

    #[test]
    fn the_peek_setters_find_the_instance_and_clamp() {
        let temp = TestDir::new();
        let a = temp.bridge("a");
        let mut config = TrayConfig::default();
        config.add(&a).unwrap();
        assert!(config.set_show_peek(&a, false));
        assert!(!config.find(&a).unwrap().show_peek);
        assert!(config.set_peek_messages(&a, 42));
        assert_eq!(config.find(&a).unwrap().peek_messages, 42);
        assert!(config.set_peek_messages(&a, 1));
        assert_eq!(config.find(&a).unwrap().peek_messages, PEEK_MESSAGES_MIN);
        assert!(config.set_peek_messages(&a, 99));
        assert_eq!(config.find(&a).unwrap().peek_messages, PEEK_MESSAGES_MAX);
        let missing = temp.path().join("missing");
        assert!(!config.set_show_peek(&missing, true));
        assert!(!config.set_peek_messages(&missing, 20));
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let temp = TestDir::new();
        let path = temp.path().join("tray.json");
        std::fs::write(&path, "old").unwrap();
        TrayConfig::default().save(&path).unwrap();
        let names: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tray.json"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(TrayConfig::load(&path).unwrap(), TrayConfig::default());
    }
}
