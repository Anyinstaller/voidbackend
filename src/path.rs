use anyhow::{bail, Context, Result};
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::WindowsProgramming::{DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_UNKNOWN};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
    FOLDERID_ProgramFilesX86, FOLDERID_Programs, FOLDERID_Windows,
};

use crate::winutil;

pub const MAX_INSTALL_PATH_UTF16: usize = 32_767;
pub const MAX_PACKAGE_PATH_UTF16: usize = 32_767;
pub const MAX_SANITIZED_NAME_CHARS: usize = 120;

/// Expand `%VAR%` references exactly once with the current process environment.
/// Unresolvable or malformed references are fatal.
pub fn expand_environment(input: &str) -> Result<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '%' {
            let start = index;
            index += 1;
            while index < chars.len() && chars[index] != '%' {
                index += 1;
            }
            if index == chars.len() {
                bail!("unterminated environment variable reference in {input:?}");
            }
            let name: String = chars[start + 1..index].iter().collect();
            if name.is_empty() {
                bail!("empty environment variable reference in {input:?}");
            }
            let value = std::env::var(&name)
                .with_context(|| format!("environment variable %{name}% is not defined"))?;
            output.push_str(&value);
            index += 1;
        } else {
            output.push(chars[index]);
            index += 1;
        }
    }
    Ok(output)
}

/// Validate an install location and return an expanded, normalized local-drive
/// path. `session_root` is the extracted package root, which must never be the
/// installation target.
pub fn validate_install_location(input: &str, session_root: Option<&Path>) -> Result<String> {
    if input.is_empty() {
        bail!("install location is empty");
    }
    if input.encode_utf16().count() > MAX_INSTALL_PATH_UTF16 {
        bail!("install location is too long");
    }
    let expanded = expand_environment(input)?;
    if expanded.encode_utf16().count() > MAX_INSTALL_PATH_UTF16 {
        bail!("expanded install location is too long");
    }
    validate_absolute_shape(&expanded)?;
    let normalized = winutil::full_path(&expanded)?;
    if normalized.encode_utf16().count() > MAX_INSTALL_PATH_UTF16 {
        bail!("normalized install location is too long");
    }
    validate_absolute_shape(&normalized)?;
    reject_non_local_drive(&normalized)?;
    reject_protected_locations(&normalized, session_root)?;
    winutil::ensure_no_reparse_components(Path::new(&normalized))?;
    Ok(normalized)
}

fn validate_absolute_shape(input: &str) -> Result<()> {
    let bytes = input.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        bail!("install location must be an absolute local drive path: {input:?}");
    }
    if bytes[2] != b'\\' && bytes[2] != b'/' {
        bail!("install location must not be drive-relative: {input:?}");
    }
    if input.contains('\0') {
        bail!("install location must not contain NUL");
    }

    let body = input[3..].trim_end_matches(['\\', '/']);
    for component in body.split(['\\', '/']) {
        if component.is_empty() {
            bail!("install location contains an empty component: {input:?}");
        }
        if component == "." || component == ".." {
            bail!("install location contains a relative component: {input:?}");
        }
        if component.ends_with('.') || component.ends_with(' ') {
            bail!("install location component ends with a dot or space: {component:?}");
        }
        if component.contains(':') {
            bail!("install location must not contain an alternate data stream: {input:?}");
        }
        if component
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
        {
            bail!("install location contains an invalid character: {input:?}");
        }
        if is_device_name(component) {
            bail!("install location contains a reserved device name: {component:?}");
        }
    }
    Ok(())
}

fn reject_non_local_drive(path: &str) -> Result<()> {
    let drive = path.as_bytes()[0] as char;
    let root = winutil::wide_str(&format!("{drive}:\\"));
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    if matches!(drive_type, DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR | DRIVE_REMOTE) {
        bail!("install location must be on a local drive: {path:?}");
    }
    Ok(())
}

fn reject_protected_locations(path: &str, session_root: Option<&Path>) -> Result<()> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
        bail!("install location must not be a drive root: {path:?}");
    }

    let roots = [
        (FOLDERID_Windows, true, "the Windows directory"),
        (FOLDERID_ProgramFiles, false, "the Program Files root"),
        (
            FOLDERID_ProgramFilesX86,
            false,
            "the Program Files (x86) root",
        ),
        (FOLDERID_ProgramData, false, "the ProgramData root"),
        (FOLDERID_Profile, false, "the user profile root"),
        (FOLDERID_Desktop, false, "the Desktop root"),
        (FOLDERID_Programs, false, "the Start Menu root"),
    ];
    for (folder, include_children, description) in roots {
        let root = winutil::known_folder(&folder)
            .with_context(|| format!("failed to resolve {description}"))?;
        let root = root.to_string_lossy();
        if same_or_under(path, &root, include_children) {
            bail!("install location must not be {description}: {path:?}");
        }
    }

    if let Some(session_root) = session_root {
        let mut forbidden_roots = vec![session_root.to_path_buf()];
        if let Some(sessions_root) = sessions_tree_root(session_root) {
            forbidden_roots.push(sessions_root.to_path_buf());
        }
        for root in forbidden_roots {
            let root = root.to_string_lossy();
            if same_or_under(path, &root, true) || same_or_under(&root, path, true) {
                bail!("install location must not overlap the session directory tree");
            }
        }
    }
    Ok(())
}

fn sessions_tree_root(session_root: &Path) -> Option<&Path> {
    let sessions = session_root.parent()?;
    let sessions_name = sessions.file_name()?.to_str()?;
    if !sessions_name.eq_ignore_ascii_case("Sessions") {
        return None;
    }
    let anyinstaller = sessions.parent()?.file_name()?.to_str()?;
    anyinstaller
        .eq_ignore_ascii_case("Anyinstaller")
        .then_some(sessions)
}

fn normalize_for_compare(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

pub fn same_or_under(path: &str, root: &str, include_children: bool) -> bool {
    let path = normalize_for_compare(path);
    let root = normalize_for_compare(root);
    if path == root {
        return true;
    }
    include_children
        && path
            .strip_prefix(&root)
            .is_some_and(|rest| rest.starts_with('\\'))
}

/// Validate a path relative to the extracted package root. The returned value
/// always uses `/` separators.
pub fn validate_package_relative(input: &str) -> Result<String> {
    if input.is_empty() {
        bail!("package path is empty");
    }
    if input.encode_utf16().count() > MAX_PACKAGE_PATH_UTF16 {
        bail!("package path is too long");
    }
    if input.starts_with('/') || input.starts_with('\\') {
        bail!("package path must be relative: {input:?}");
    }

    let mut normalized = String::with_capacity(input.len());
    for (index, raw_component) in input.split(['/', '\\']).enumerate() {
        if index > 0 {
            normalized.push('/');
        }
        if raw_component.is_empty() {
            bail!("package path contains an empty component: {input:?}");
        }
        if raw_component == "." || raw_component == ".." {
            bail!("package path contains a relative component: {input:?}");
        }
        if raw_component.ends_with('.') || raw_component.ends_with(' ') {
            bail!("package path component ends with a dot or space: {raw_component:?}");
        }
        if raw_component.contains(':') {
            bail!("package path must not contain a drive or alternate data stream: {input:?}");
        }
        if raw_component
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
        {
            bail!("package path contains an invalid character: {input:?}");
        }
        if is_device_name(raw_component) {
            bail!("package path contains a reserved device name: {raw_component:?}");
        }
        normalized.push_str(raw_component);
    }
    Ok(normalized)
}

/// Sanitize an application name for use as a single Windows file or directory
/// component. No caller-supplied path separators remain after this function.
pub fn sanitize_file_name(name: &str) -> String {
    let mut value = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            value.push('_');
        } else {
            value.push(ch);
        }
    }
    value = value.trim_end_matches(['.', ' ']).to_string();
    if value.chars().count() > MAX_SANITIZED_NAME_CHARS {
        value = value.chars().take(MAX_SANITIZED_NAME_CHARS).collect();
        value = value.trim_end_matches(['.', ' ']).to_string();
    }
    if value.is_empty() {
        value.push_str("Application");
    }
    if is_device_name(&value) {
        value.insert(0, '_');
    }
    value
}

pub fn is_device_name(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.as_bytes()[3].is_ascii_digit()
        && upper.as_bytes()[3] != b'0')
}

pub fn is_special_background(value: &str) -> bool {
    [
        "acrylic",
        "acrylicblur",
        "mica",
        "windowsmanaged",
        "transparent",
    ]
    .iter()
    .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

pub fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_environment_once() {
        std::env::set_var("VOIDBACKEND_TEST_VAR", "value");
        assert_eq!(
            expand_environment(r"C:\%VOIDBACKEND_TEST_VAR%").unwrap(),
            r"C:\value"
        );
        assert!(expand_environment("C:\\%VOIDBACKEND_MISSING%").is_err());
        assert!(expand_environment("C:\\%BROKEN").is_err());
    }

    fn current_drive() -> char {
        let current = std::env::current_dir().unwrap();
        current.to_string_lossy().as_bytes()[0] as char
    }

    #[test]
    fn accepts_valid_install_locations() {
        let drive = current_drive();
        assert!(validate_install_location(&format!("{drive}:\\Apps\\Example"), None).is_ok());
        let program_files = std::env::var("ProgramFiles").unwrap();
        assert!(validate_install_location(&format!("{program_files}\\Example"), None).is_ok());
        assert!(validate_install_location(r"%LOCALAPPDATA%\Programs\Example", None).is_ok());
    }

    #[test]
    fn rejects_unsafe_install_locations() {
        let drive = current_drive();
        let system_root = std::env::var("SystemRoot").unwrap();
        let paths = vec![
            r"\\server\share\app".to_string(),
            format!(r"\\.\{drive}:\app"),
            format!(r"\\?\{drive}:\app"),
            format!("{drive}:app"),
            r"app\sub".to_string(),
            format!(r"{drive}:\Apps\..\Windows"),
            format!(r"{drive}:\Apps\file:stream"),
            format!(r"{drive}:\Apps\CON"),
            format!(r"{drive}:\Apps\trailing."),
            r"C:\Apps\%VOIDBACKEND_MISSING%".to_string(),
            String::new(),
            format!("{drive}:\\"),
            system_root.clone(),
            format!("{system_root}\\System32"),
        ];
        for path in paths {
            assert!(
                validate_install_location(&path, None).is_err(),
                "expected rejection: {path:?}"
            );
        }
    }

    #[test]
    fn rejects_sibling_session_directories() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("Anyinstaller").join("Sessions");
        let current = sessions.join("current");
        std::fs::create_dir_all(&current).unwrap();
        let sibling = sessions.join("sibling").to_string_lossy().into_owned();
        assert!(validate_install_location(&sibling, Some(&current)).is_err());
    }

    #[test]
    fn rejects_session_directory_overlap_in_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session");
        std::fs::create_dir(&session).unwrap();
        let ancestor = root.path().to_string_lossy().into_owned();
        let child = session.join("install").to_string_lossy().into_owned();
        assert!(validate_install_location(&ancestor, Some(&session)).is_err());
        assert!(validate_install_location(&child, Some(&session)).is_err());
    }

    #[test]
    fn normalizes_package_paths() {
        assert_eq!(
            validate_package_relative(r"assets\background.avif").unwrap(),
            "assets/background.avif"
        );
        for path in [
            "",
            "/absolute",
            r"C:\absolute",
            r"..\escape",
            "a/../b",
            "a//b",
            "a/b.",
            "a/NUL",
            "a/file:stream",
            "a/<bad>",
            "./file",
        ] {
            assert!(
                validate_package_relative(path).is_err(),
                "expected rejection: {path:?}"
            );
        }
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name(r"A/B:C*?"), "A_B_C__");
        assert_eq!(sanitize_file_name("name..."), "name");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name(""), "Application");
    }

    #[test]
    fn compares_paths_case_insensitively() {
        assert!(same_or_under(
            r"c:\apps\example\bin",
            r"C:\Apps\Example",
            true
        ));
        assert!(!same_or_under(
            r"C:\Apps\Example2",
            r"C:\Apps\Example",
            true
        ));
        assert!(same_or_under(r"C:\Apps\Example", r"C:\Apps\Example", false));
        assert!(!same_or_under(
            r"C:\Apps\Example\bin",
            r"C:\Apps\Example",
            false
        ));
    }

    #[test]
    fn background_special_values() {
        assert!(is_special_background("AcrylicBlur"));
        assert!(is_hex_color("#1e90ff"));
        assert!(!is_hex_color("#fff"));
        assert!(!is_hex_color("#GGGGGG"));
    }
}
