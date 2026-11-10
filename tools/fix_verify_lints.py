"""Fixes the lints the digest work introduced."""

from pathlib import Path

HEX_OLD = """    let mut hex = String::new();
    for byte in hasher.finalize() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex"""
HEX_NEW = """    let mut hex = String::new();
    for byte in hasher.finalize() {
        // Writing into a `String` cannot fail, and the alternative is an allocation per byte.
        let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
    }
    hex"""

# 1. ncui-native: the installer and its test helpers.
p = Path('crates/ncui-native/src/install.rs')
s = p.read_text(encoding='utf-8')

old = """    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)"""
new = """    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        // Writing into a `String` cannot fail, and the alternative is an allocation per byte.
        let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
    }
    Ok(hex)"""
assert old in s, 'installer digest not found'
s = s.replace(old, new, 1)

assert HEX_OLD in s, 'test digest not found'
s = s.replace(HEX_OLD, HEX_NEW, 1)

s = s.replace('fn test_digest(path: &std::path::Path) -> String {', 'fn test_digest(path: &Path) -> String {', 1)
s = s.replace('fn test_records(root: &std::path::Path, paths: &[&str]) -> serde_json::Value {',
              'fn test_records(root: &Path, paths: &[&str]) -> serde_json::Value {', 1)

# 2. Extract the per-file check so `verify_tree` fits.
old = """    for file in &manifest.files {
        let full = directory.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let metadata = std::fs::metadata(&full).map_err(|error| InstallError::Damaged {
            path: file.path.clone(),
            reason: format!("the bundle names it and it is not there: {error}"),
        })?;
        if metadata.len() != file.bytes {
            return Err(InstallError::Damaged {
                path: file.path.clone(),
                reason: format!(
                    "the bundle says {} bytes and it is {}",
                    file.bytes,
                    metadata.len()
                ),
            });
        }
        let digest = digest_of(&full)?;
        if !digest.eq_ignore_ascii_case(&file.sha256) {
            return Err(InstallError::Damaged {
                path: file.path.clone(),
                reason: format!(
                    "the bundle says its digest is {} and it is {digest}",
                    file.sha256
                ),
            });
        }
    }"""
new = """    for file in &manifest.files {
        check_file(directory, file)?;
    }"""
assert old in s, 'verify loop not found'
s = s.replace(old, new, 1)

old = """/// Every file under `directory`, as paths relative to `root` with forward slashes."""
new = """/// Checks one file against the record the bundle carries for it.
///
/// # Errors
///
/// Returns [`InstallError::Damaged`] when the file is missing, is a different size, or hashes to
/// something else, and [`InstallError::Io`] when it cannot be read.
fn check_file(directory: &Path, file: &BundleFile) -> Result<(), InstallError> {
    let full = directory.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let metadata = std::fs::metadata(&full).map_err(|error| InstallError::Damaged {
        path: file.path.clone(),
        reason: format!("the bundle names it and it is not there: {error}"),
    })?;
    if metadata.len() != file.bytes {
        return Err(InstallError::Damaged {
            path: file.path.clone(),
            reason: format!(
                "the bundle says {} bytes and it is {}",
                file.bytes,
                metadata.len()
            ),
        });
    }
    let digest = digest_of(&full)?;
    if !digest.eq_ignore_ascii_case(&file.sha256) {
        return Err(InstallError::Damaged {
            path: file.path.clone(),
            reason: format!(
                "the bundle says its digest is {} and it is {digest}",
                file.sha256
            ),
        });
    }
    Ok(())
}

/// Every file under `directory`, as paths relative to `root` with forward slashes."""
assert old in s, 'walk anchor not found'
s = s.replace(old, new, 1)
p.write_text(s, encoding='utf-8')

# 3. The CLI's test helpers have the same shape.
for path in ['crates/ncui-cli/src/lib.rs', 'crates/ncui-cli/tests/installer.rs']:
    p = Path(path)
    s = p.read_text(encoding='utf-8')
    if HEX_OLD in s:
        s = s.replace(HEX_OLD, HEX_NEW, 1)
        p.write_text(s, encoding='utf-8')
        print(f'{path}: hex fixed')

print('lints fixed')
