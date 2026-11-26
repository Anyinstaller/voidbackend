use anyhow::{bail, Context, Result};
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, WIN32_ERROR};
use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, GetFullPathNameW, SetFileAttributesW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
    INVALID_FILE_ATTRIBUTES,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};

pub const FILE_ATTRIBUTE_REPARSE_POINT_BITS: u32 = FILE_ATTRIBUTE_REPARSE_POINT.0;

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn is_win32(error: &windows::core::Error, code: WIN32_ERROR) -> bool {
    error.code() == windows::core::HRESULT::from_win32(code.0)
}

/// Add the Win32 verbatim prefix so path APIs do not apply legacy MAX_PATH
/// normalization. The input must already be absolute and normalized.
pub fn verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    if text.len() >= 3 && text.as_bytes()[1] == b':' {
        return PathBuf::from(format!(r"\\?\{text}"));
    }
    path.to_path_buf()
}

pub fn full_path(input: &str) -> Result<String> {
    let wide_input = wide_str(input);
    let mut buffer = vec![0u16; 512];
    loop {
        let length = unsafe {
            GetFullPathNameW(
                PCWSTR(wide_input.as_ptr()),
                Some(buffer.as_mut_slice()),
                None,
            )
        };
        if length == 0 {
            return Err(windows::core::Error::from_thread())
                .with_context(|| format!("GetFullPathNameW failed for {input:?}"));
        }
        let length = length as usize;
        if length < buffer.len() {
            let text = String::from_utf16(&buffer[..length])
                .context("normalized path is not valid UTF-16")?;
            return Ok(text);
        }
        buffer.resize(length + 1, 0);
    }
}

pub fn attributes_if_exists(path: &Path) -> Result<Option<FILE_FLAGS_AND_ATTRIBUTES>> {
    let wide_path = wide(verbatim(path));
    let attributes = unsafe { GetFileAttributesW(PCWSTR(wide_path.as_ptr())) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        let error = windows::core::Error::from_thread();
        if is_win32(&error, ERROR_FILE_NOT_FOUND) || is_win32(&error, ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("GetFileAttributesW failed for {}", path.display()));
    }
    Ok(Some(FILE_FLAGS_AND_ATTRIBUTES(attributes)))
}

pub fn clear_readonly(path: &Path) -> Result<()> {
    let Some(attributes) = attributes_if_exists(path)? else {
        return Ok(());
    };
    let attributes = FILE_FLAGS_AND_ATTRIBUTES(attributes.0 & !FILE_ATTRIBUTE_READONLY.0);
    let wide_path = wide(verbatim(path));
    unsafe { SetFileAttributesW(PCWSTR(wide_path.as_ptr()), attributes) }
        .with_context(|| format!("SetFileAttributesW failed for {}", path.display()))
}

pub fn is_reparse_point(attributes: FILE_FLAGS_AND_ATTRIBUTES) -> bool {
    attributes.0 & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

pub fn is_directory(attributes: FILE_FLAGS_AND_ATTRIBUTES) -> bool {
    attributes.0 & FILE_ATTRIBUTE_DIRECTORY.0 != 0
}

/// Validate every existing component from the volume root to `path`. Missing
/// trailing components are allowed; a missing component cannot contain an
/// existing descendant.
pub fn ensure_no_reparse_components(path: &Path) -> Result<()> {
    let ancestors: Vec<&Path> = path
        .ancestors()
        .filter(|ancestor| ancestor.has_root())
        .collect();
    for ancestor in ancestors.into_iter().rev() {
        let Some(attributes) = attributes_if_exists(ancestor)? else {
            break;
        };
        if is_reparse_point(attributes) {
            bail!("path contains a reparse point: {}", ancestor.display());
        }
        if ancestor != path && !is_directory(attributes) {
            bail!("path parent is not a directory: {}", ancestor.display());
        }
    }
    Ok(())
}

pub fn require_directory(path: &Path, label: &str) -> Result<FILE_FLAGS_AND_ATTRIBUTES> {
    let some = attributes_if_exists(path)?
        .with_context(|| format!("{label} does not exist: {}", path.display()))?;
    if !is_directory(some) {
        bail!("{label} is not a directory: {}", path.display());
    }
    if is_reparse_point(some) {
        bail!("{label} must not be a reparse point: {}", path.display());
    }
    Ok(some)
}

pub fn require_regular_file(path: &Path, label: &str) -> Result<FILE_FLAGS_AND_ATTRIBUTES> {
    let some = attributes_if_exists(path)?
        .with_context(|| format!("{label} does not exist: {}", path.display()))?;
    if is_directory(some) {
        bail!("{label} is a directory: {}", path.display());
    }
    if is_reparse_point(some) {
        bail!("{label} must not be a reparse point: {}", path.display());
    }
    Ok(some)
}

pub fn known_folder(folder: &windows::core::GUID) -> Result<PathBuf> {
    let raw: PWSTR = unsafe {
        SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None)
            .context("SHGetKnownFolderPath failed")?
    };
    let result = unsafe { raw.to_string() }.context("known folder path is not valid UTF-16");
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
    let path = result?;
    if path.is_empty() {
        bail!("known folder path is empty");
    }
    Ok(PathBuf::from(path))
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("random generator failed: {error}"))?;
    Ok(bytes)
}

pub fn random_hex<const N: usize>() -> Result<String> {
    let bytes = random_bytes::<N>()?;
    let mut out = String::with_capacity(N * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_absolute_paths() {
        let current = std::env::current_dir().unwrap();
        let normalized = full_path(&current.to_string_lossy()).unwrap();
        assert!(normalized.len() >= 3);
        assert_eq!(normalized.as_bytes()[1], b':');
    }

    #[test]
    fn random_hex_has_expected_length() {
        let value = random_hex::<16>().unwrap();
        assert_eq!(value.len(), 32);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
