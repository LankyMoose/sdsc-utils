//! OS appearance for the in-Start browser (`prefers-color-scheme`).
//!
//! Windows reads the personalize key directly. Other platforms return
//! `None` (leave Servo's default alone) until they grow a native readout.

/// True when the OS is in dark mode, `None` when unknown.
pub fn system_dark_mode() -> Option<bool> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        let light: u32 = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
            .and_then(|key| key.get_value("AppsUseLightTheme"))
            .unwrap_or(1);
        Some(light == 0)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_mode_read_returns_without_panic() {
        // No assertion on the value (machine-dependent); the read itself
        // must never panic or block.
        let _ = system_dark_mode();
    }
}
