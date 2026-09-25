//! Detect Microsoft Store / MSIX package identity (Windows).

/// Returns true when this process has an MSIX / Store package identity.
pub fn is_packaged() -> bool {
    #[cfg(windows)]
    {
        is_packaged_windows()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn is_packaged_windows() -> bool {
    // Unpackaged: APPMODEL_ERROR_NO_PACKAGE. Packaged: ERROR_INSUFFICIENT_BUFFER + length.
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentPackageFullName(
            package_full_name_length: *mut u32,
            package_full_name: *mut u16,
        ) -> i32;
    }

    let mut len: u32 = 0;
    let rc = unsafe { GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) };
    // Unpackaged: APPMODEL_ERROR_NO_PACKAGE. Packaged: ERROR_INSUFFICIENT_BUFFER + length.
    rc == ERROR_INSUFFICIENT_BUFFER && len > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpackaged_dev_build_is_not_packaged() {
        // Unit tests run as a normal cargo binary, not inside an MSIX.
        assert!(!is_packaged());
    }
}
