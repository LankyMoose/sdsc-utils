//! Portable builds append `sdsc-shell` to `sdsc-utils` so one download is enough.
//!
//! Windows (and other loaders) ignore bytes after the executable image. When a
//! sibling shell is already next to the service — dev builds and the MSIX — that
//! file is used and nothing is unpacked.

use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Footer magic. `1` is the layout version.
const BUNDLE_MAGIC: &[u8; 8] = b"SDSCSHL1";
const SHA_LEN: usize = 32;
const LEN_LEN: usize = 8;
const MAGIC_LEN: usize = 8;
const FOOTER_LEN: usize = SHA_LEN + LEN_LEN + MAGIC_LEN;
/// Refuse a corrupt length instead of allocating a huge buffer.
const MAX_PAYLOAD_LEN: u64 = 64 * 1024 * 1024;

struct BundleFooter {
    sha256: [u8; SHA_LEN],
    payload_len: u64,
}

fn shell_file_name() -> &'static str {
    if cfg!(windows) {
        "sdsc-shell.exe"
    } else {
        "sdsc-shell"
    }
}

fn diag(message: &str) {
    #[cfg(debug_assertions)]
    crate::platform::app_log::info(format!("shell-bundle: {message}"));
    #[cfg(not(debug_assertions))]
    let _ = message;
}

/// Append `shell` onto a copy of `service` at `out`.
pub fn append_shell_bundle(service: &Path, shell: &Path, out: &Path) -> Result<(), String> {
    if service == out || shell == out {
        return Err("out path must differ from the service and shell executables".into());
    }
    if read_footer(service)?.is_some() {
        return Err(format!("{} already has a bundled shell", service.display()));
    }
    let shell_bytes = fs::read(shell).map_err(|e| format!("read {}: {e}", shell.display()))?;
    let payload_len =
        u64::try_from(shell_bytes.len()).map_err(|_| "shell too large".to_string())?;
    if payload_len == 0 || payload_len > MAX_PAYLOAD_LEN {
        return Err(format!(
            "shell size {payload_len} is outside the bundle limit"
        ));
    }
    let digest = sha256_bytes(&shell_bytes);

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut src = File::open(service).map_err(|e| format!("read {}: {e}", service.display()))?;
    let mut dst = File::create(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    std::io::copy(&mut src, &mut dst).map_err(|e| format!("copy service: {e}"))?;
    dst.write_all(&shell_bytes)
        .map_err(|e| format!("write shell payload: {e}"))?;
    dst.write_all(&digest)
        .map_err(|e| format!("write bundle footer: {e}"))?;
    dst.write_all(&payload_len.to_le_bytes())
        .map_err(|e| format!("write bundle footer: {e}"))?;
    dst.write_all(BUNDLE_MAGIC)
        .map_err(|e| format!("write bundle footer: {e}"))?;
    Ok(())
}

/// Shell path for this process: sibling exe, or the copy unpacked from the bundle.
pub fn prepare_shell_exe() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let cache = crate::persist::paths::data_dir().join("shell");
    resolve_shell_exe(&current, &cache)
}

/// Pick the shell for `service_exe`, unpacking into `cache_dir` when needed.
pub fn resolve_shell_exe(service_exe: &Path, cache_dir: &Path) -> Result<PathBuf, String> {
    let sibling = service_exe.with_file_name(shell_file_name());
    match read_footer(service_exe)? {
        None => {
            if sibling.is_file() {
                diag(&format!("using sibling {}", sibling.display()));
                Ok(sibling)
            } else {
                Err(format!(
                    "sdsc-shell not found next to {} and this executable has no bundled shell",
                    service_exe.display()
                ))
            }
        }
        Some(footer) => {
            if sibling.is_file() && file_matches(&sibling, &footer)? {
                diag(&format!("using sibling {}", sibling.display()));
                return Ok(sibling);
            }
            cached_or_extract(service_exe, cache_dir, &footer)
        }
    }
}

fn cached_or_extract(
    service_exe: &Path,
    cache_dir: &Path,
    footer: &BundleFooter,
) -> Result<PathBuf, String> {
    let dest = cache_dir.join(shell_file_name());
    let stamp = cache_dir.join("sdsc-shell.sha256");
    let expected = sha_hex(&footer.sha256);
    if dest.is_file()
        && file_len(&dest)? == footer.payload_len
        && fs::read_to_string(&stamp).ok().as_deref().map(str::trim) == Some(expected.as_str())
    {
        diag(&format!("using cached {}", dest.display()));
        return Ok(dest);
    }

    let payload = read_payload(service_exe, footer)?;
    let digest = sha256_bytes(&payload);
    if digest != footer.sha256 {
        return Err("bundled shell checksum mismatch".into());
    }

    fs::create_dir_all(cache_dir).map_err(|e| format!("create {}: {e}", cache_dir.display()))?;
    let partial = cache_dir.join(format!("{}.partial", shell_file_name()));
    fs::write(&partial, &payload).map_err(|e| format!("write {}: {e}", partial.display()))?;
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| format!("replace {}: {e}", dest.display()))?;
    }
    fs::rename(&partial, &dest).map_err(|e| format!("rename {}: {e}", dest.display()))?;
    fs::write(&stamp, &expected).map_err(|e| format!("write {}: {e}", stamp.display()))?;
    crate::platform::app_log::info(format!("extracted bundled shell to {}", dest.display()));
    Ok(dest)
}

fn read_payload(service_exe: &Path, footer: &BundleFooter) -> Result<Vec<u8>, String> {
    let mut file =
        File::open(service_exe).map_err(|e| format!("read {}: {e}", service_exe.display()))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", service_exe.display()))?
        .len();
    let start = file_len - FOOTER_LEN as u64 - footer.payload_len;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| format!("seek bundle payload: {e}"))?;
    let mut payload = vec![0u8; footer.payload_len as usize];
    file.read_exact(&mut payload)
        .map_err(|e| format!("read bundle payload: {e}"))?;
    Ok(payload)
}

fn file_matches(path: &Path, footer: &BundleFooter) -> Result<bool, String> {
    if file_len(path)? != footer.payload_len {
        return Ok(false);
    }
    Ok(sha256_file(path)? == footer.sha256)
}

fn file_len(path: &Path) -> Result<u64, String> {
    Ok(fs::metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len())
}

fn read_footer(path: &Path) -> Result<Option<BundleFooter>, String> {
    let mut file = File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if len < FOOTER_LEN as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))
        .map_err(|e| format!("seek bundle footer: {e}"))?;
    let mut footer = [0u8; FOOTER_LEN];
    file.read_exact(&mut footer)
        .map_err(|e| format!("read bundle footer: {e}"))?;
    if &footer[SHA_LEN + LEN_LEN..] != BUNDLE_MAGIC {
        return Ok(None);
    }
    let payload_len = u64::from_le_bytes(
        footer[SHA_LEN..SHA_LEN + LEN_LEN]
            .try_into()
            .expect("payload len is 8 bytes"),
    );
    if payload_len == 0 || payload_len > MAX_PAYLOAD_LEN {
        return Err(format!("bundled shell length {payload_len} is invalid"));
    }
    if len < payload_len + FOOTER_LEN as u64 {
        return Err("bundled shell extends past the start of the file".into());
    }
    let mut sha256 = [0u8; SHA_LEN];
    sha256.copy_from_slice(&footer[..SHA_LEN]);
    Ok(Some(BundleFooter {
        sha256,
        payload_len,
    }))
}

fn sha256_bytes(bytes: &[u8]) -> [u8; SHA_LEN] {
    Sha256::digest(bytes).into()
}

fn sha256_file(path: &Path) -> Result<[u8; SHA_LEN], String> {
    let mut file = File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("hash {}: {e}", path.display()))?;
    Ok(hasher.finalize().into())
}

fn sha_hex(bytes: &[u8; SHA_LEN]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(SHA_LEN * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("sdsc-shell-bundle-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn sibling_is_used_when_the_service_has_no_bundle() {
        let dir = TempDir::new("sibling");
        let service = dir.path().join("sdsc-utils.exe");
        let shell = dir.path().join(shell_file_name());
        write(&service, b"service-image");
        write(&shell, b"shell-image");

        let resolved = resolve_shell_exe(&service, &dir.path().join("cache")).unwrap();
        assert_eq!(resolved, shell);
        assert!(!dir.path().join("cache").exists());
    }

    #[test]
    fn missing_sibling_without_a_bundle_is_an_error() {
        let dir = TempDir::new("missing");
        let service = dir.path().join("sdsc-utils.exe");
        write(&service, b"service-image");

        let err = resolve_shell_exe(&service, &dir.path().join("cache")).unwrap_err();
        assert!(err.contains("no bundled shell"), "{err}");
    }

    #[test]
    fn bundle_unpacks_when_no_sibling_is_present() {
        let dir = TempDir::new("unpack");
        let service = dir.path().join("sdsc-utils.exe");
        let shell = dir.path().join("payload.bin");
        let bundled = dir.path().join("portable.exe");
        write(&service, b"service-image");
        write(&shell, b"shell-bytes-v1");
        append_shell_bundle(&service, &shell, &bundled).unwrap();

        let cache = dir.path().join("cache");
        let resolved = resolve_shell_exe(&bundled, &cache).unwrap();
        assert_eq!(resolved, cache.join(shell_file_name()));
        assert_eq!(fs::read(&resolved).unwrap(), b"shell-bytes-v1");

        let again = resolve_shell_exe(&bundled, &cache).unwrap();
        assert_eq!(again, resolved);
        assert!(
            !cache
                .join(format!("{}.partial", shell_file_name()))
                .exists()
        );
    }

    #[test]
    fn matching_sibling_wins_over_the_bundle() {
        let dir = TempDir::new("match");
        let service = dir.path().join("sdsc-utils.exe");
        let shell = dir.path().join("payload.bin");
        write(&service, b"service-image");
        write(&shell, b"same-shell");
        let bundled_dir = dir.path().join("installed");
        fs::create_dir_all(&bundled_dir).unwrap();
        let bundled = bundled_dir.join("sdsc-utils.exe");
        append_shell_bundle(&service, &shell, &bundled).unwrap();
        write(&bundled_dir.join(shell_file_name()), b"same-shell");

        let cache = dir.path().join("cache");
        let resolved = resolve_shell_exe(&bundled, &cache).unwrap();
        assert_eq!(resolved, bundled_dir.join(shell_file_name()));
        assert!(!cache.exists());
    }

    #[test]
    fn stale_sibling_is_replaced_by_the_bundled_shell() {
        let dir = TempDir::new("stale");
        let service = dir.path().join("sdsc-utils.exe");
        let shell = dir.path().join("payload.bin");
        write(&service, b"service-image");
        write(&shell, b"new-shell");
        let bundled_dir = dir.path().join("installed");
        fs::create_dir_all(&bundled_dir).unwrap();
        let bundled = bundled_dir.join("sdsc-utils.exe");
        append_shell_bundle(&service, &shell, &bundled).unwrap();
        write(&bundled_dir.join(shell_file_name()), b"old-shell");

        let cache = dir.path().join("cache");
        let resolved = resolve_shell_exe(&bundled, &cache).unwrap();
        assert_eq!(fs::read(&resolved).unwrap(), b"new-shell");
        assert_ne!(resolved, bundled_dir.join(shell_file_name()));
    }

    #[test]
    fn appending_twice_is_rejected() {
        let dir = TempDir::new("twice");
        let service = dir.path().join("sdsc-utils.exe");
        let shell = dir.path().join("payload.bin");
        let once = dir.path().join("once.exe");
        let twice = dir.path().join("twice.exe");
        write(&service, b"service-image");
        write(&shell, b"shell");
        append_shell_bundle(&service, &shell, &once).unwrap();
        let err = append_shell_bundle(&once, &shell, &twice).unwrap_err();
        assert!(err.contains("already has a bundled shell"), "{err}");
    }

    #[test]
    fn corrupt_payload_fails_the_checksum() {
        let dir = TempDir::new("corrupt");
        let service = dir.path().join("sdsc-utils.exe");
        let shell = dir.path().join("payload.bin");
        let bundled = dir.path().join("portable.exe");
        write(&service, b"service-image");
        write(&shell, b"shell-bytes");
        append_shell_bundle(&service, &shell, &bundled).unwrap();

        let mut bytes = fs::read(&bundled).unwrap();
        let flip_at = b"service-image".len();
        bytes[flip_at] ^= 0xff;
        fs::write(&bundled, &bytes).unwrap();

        let err = resolve_shell_exe(&bundled, &dir.path().join("cache")).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
    }
}
