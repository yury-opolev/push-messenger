//! The taskbar's theme, which colours the tray glyph.

use push_tray_core::icon::Theme;

/// How often the theme is read again.
pub const POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// The taskbar's theme: `SystemUsesLightTheme` (`HKCU\…\Themes\Personalize`) 1 is light,
/// anything else (or no value) dark.
#[cfg(windows)]
pub fn current() -> Theme {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let light = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_value::<u32, _>("SystemUsesLightTheme"))
        .is_ok_and(|value| value == 1);
    if light { Theme::Light } else { Theme::Dark }
}

/// Dark until the Linux panel theme is wired.
#[cfg(not(windows))]
pub fn current() -> Theme {
    Theme::Dark
}
