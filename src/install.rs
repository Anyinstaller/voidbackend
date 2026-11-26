use anyhow::{bail, Context, Result};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::acl;
use crate::config::Config;
use crate::log;
use crate::message::InstallSettings;
use crate::path;
use crate::payload;
use crate::shortcut::{self, ShortcutSpec};
use crate::winutil;

const MAX_STAGE_ATTEMPTS: usize = 32;
const MAX_ICON_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub install_location: PathBuf,
    pub launch_target: Option<PathBuf>,
    pub files: usize,
    pub total_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    Validating,
    Preparing,
    Extracting,
    Verifying,
    Committing,
    CreatingShortcuts,
    Finishing,
}

impl InstallStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validating => "validating",
            Self::Preparing => "preparing",
            Self::Extracting => "extracting",
            Self::Verifying => "verifying",
            Self::Committing => "committing",
            Self::CreatingShortcuts => "creatingShortcuts",
            Self::Finishing => "finishing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallProgress {
    pub stage: InstallStage,
    pub percent: u8,
    pub current: Option<u64>,
    pub total: Option<u64>,
}

struct ProgressReporter<F> {
    callback: F,
    last_percent: u8,
    last_stage: Option<InstallStage>,
}

impl<F> ProgressReporter<F>
where
    F: FnMut(InstallProgress),
{
    fn new(callback: F) -> Self {
        Self {
            callback,
            last_percent: 0,
            last_stage: None,
        }
    }

    fn emit(&mut self, stage: InstallStage, percent: u8, current: Option<u64>, total: Option<u64>) {
        let percent = percent.min(100);
        if self.last_stage == Some(stage) && percent <= self.last_percent {
            return;
        }
        let percent = percent.max(self.last_percent);
        self.last_percent = percent;
        self.last_stage = Some(stage);
        (self.callback)(InstallProgress {
            stage,
            percent,
            current,
            total,
        });
    }
}

/// Execute the complete backend installation transaction.
///
/// The destination directory is replaced as a unit, not merged. A previous
/// installation is kept in a sibling backup until the new shortcuts are in
/// place, then removed.
pub fn install(
    package_root: &Path,
    payload_path: &Path,
    config: &Config,
    settings: &InstallSettings,
) -> Result<InstallOutcome> {
    install_with_progress(package_root, payload_path, config, settings, |_| {})
}

pub fn install_with_progress<F>(
    package_root: &Path,
    payload_path: &Path,
    config: &Config,
    settings: &InstallSettings,
    progress: F,
) -> Result<InstallOutcome>
where
    F: FnMut(InstallProgress),
{
    let mut reporter = ProgressReporter::new(progress);
    reporter.emit(InstallStage::Validating, 0, None, None);

    winutil::require_directory(package_root, "package root")?;
    winutil::ensure_no_reparse_components(package_root)?;
    winutil::require_regular_file(payload_path, "payload.zip")?;

    let install_location =
        path::validate_install_location(&settings.install_location, Some(package_root))?;
    let install_location = PathBuf::from(install_location);
    let parent = install_location
        .parent()
        .context("install location has no parent directory")?;
    ensure_directory_chain(parent)?;
    winutil::ensure_no_reparse_components(&install_location)?;

    if let Some(attributes) = winutil::attributes_if_exists(&install_location)? {
        if !winutil::is_directory(attributes) {
            bail!(
                "install location exists but is not a directory: {}",
                install_location.display()
            );
        }
        if winutil::is_reparse_point(attributes) {
            bail!(
                "install location must not be a reparse point: {}",
                install_location.display()
            );
        }
    }

    reporter.emit(InstallStage::Preparing, 5, None, None);
    let stage = create_stage_directory(parent)?;
    let mut stage_guard = PathGuard::new(stage.clone());
    let contents = payload::extract_with_progress(payload_path, &stage, |current, total| {
        let percent = if total == 0 {
            10
        } else {
            ((current as u128 * 68 / total as u128) as u8).saturating_add(10)
        };
        reporter.emit(
            InstallStage::Extracting,
            percent.min(78),
            Some(current),
            Some(total),
        );
    })?;
    reporter.emit(InstallStage::Verifying, 80, None, None);

    let shortcuts_requested = settings.create_start_menu_folder || settings.create_desktop_shortcut;
    let launch_relative = if shortcuts_requested {
        Some(select_launch_target(&contents.executables, &config.name)?)
    } else {
        None
    };
    let shortcut_icon_relative = if shortcuts_requested {
        prepare_shortcut_icon(package_root, config.icon.as_deref(), &stage)
    } else {
        None
    };

    reporter.emit(InstallStage::Committing, 85, None, None);
    let mut backup_path = backup_existing_install(&install_location, parent)?;
    if let Err(error) = commit_stage(&stage, &install_location) {
        let rollback = if let Some(backup) = backup_path.take() {
            restore_backup(&install_location, backup)
        } else {
            Ok(())
        };
        return Err(with_rollback(error, rollback));
    }
    stage_guard.mark_moved();

    if let Err(error) = reset_dacl_tree(&install_location) {
        let rollback = rollback_install(&install_location, backup_path.take());
        return Err(with_rollback(error, rollback));
    }

    if let Some(relative_target) = &launch_relative {
        let target = install_location.join(relative_target);
        if let Err(error) = verify_installed_target(&target, &install_location) {
            let rollback = rollback_install(&install_location, backup_path.take());
            return Err(with_rollback(error, rollback));
        }
        reporter.emit(InstallStage::CreatingShortcuts, 92, None, None);
        let icon = shortcut_icon_relative
            .as_ref()
            .map(|relative| install_location.join(relative))
            .unwrap_or_else(|| target.clone());
        let shortcut_result = shortcut::create_shortcuts(&ShortcutSpec {
            name: config.name.clone(),
            target: target.clone(),
            icon,
            working_directory: install_location.clone(),
            create_desktop_shortcut: settings.create_desktop_shortcut,
            create_start_menu_folder: settings.create_start_menu_folder,
        });
        if let Err(error) = shortcut_result {
            let rollback = rollback_install(&install_location, backup_path.take());
            return Err(with_rollback(error, rollback));
        }
        reporter.emit(InstallStage::CreatingShortcuts, 96, None, None);
    }

    reporter.emit(InstallStage::Finishing, 99, None, None);
    if let Some(backup) = backup_path {
        if let Err(error) = remove_tree(&backup) {
            log::line(format!(
                "backend: installation succeeded but the previous installation backup could not be removed ({}): {error:#}",
                backup.display()
            ));
        }
    }
    reporter.emit(InstallStage::Finishing, 100, None, None);

    Ok(InstallOutcome {
        install_location,
        launch_target: launch_relative,
        files: contents.files,
        total_size: contents.total_size,
    })
}
/// Commit the staged tree before replacing the previous destination. This
/// split lets the caller preserve the old tree until shortcuts are complete.
fn commit_stage(stage: &Path, target: &Path) -> Result<()> {
    if winutil::attributes_if_exists(target)?.is_some() {
        bail!("install destination unexpectedly already exists");
    }
    std::fs::rename(stage, target).with_context(|| {
        format!(
            "failed to commit staged installation to {}",
            target.display()
        )
    })
}

fn backup_existing_install(target: &Path, parent: &Path) -> Result<Option<PathBuf>> {
    let Some(attributes) = winutil::attributes_if_exists(target)? else {
        return Ok(None);
    };
    if !winutil::is_directory(attributes) || winutil::is_reparse_point(attributes) {
        bail!(
            "existing install location is not a safe directory: {}",
            target.display()
        );
    }
    let backup = unique_path(parent, ".aip-backup-")?;
    if winutil::attributes_if_exists(&backup)?.is_some() {
        bail!(
            "backup destination unexpectedly exists: {}",
            backup.display()
        );
    }
    std::fs::rename(target, &backup).with_context(|| {
        format!(
            "failed to move existing installation {} to backup {}",
            target.display(),
            backup.display()
        )
    })?;
    Ok(Some(backup))
}

fn restore_backup(target: &Path, backup: PathBuf) -> Result<()> {
    if winutil::attributes_if_exists(&backup)?.is_none() {
        bail!(
            "previous installation backup is missing: {}",
            backup.display()
        );
    }
    std::fs::rename(&backup, target).with_context(|| {
        format!(
            "failed to restore previous installation from {} to {}",
            backup.display(),
            target.display()
        )
    })
}

fn rollback_install(target: &Path, backup: Option<PathBuf>) -> Result<()> {
    let remove_result = remove_tree(target);
    let Some(backup) = backup else {
        return remove_result;
    };
    if let Err(error) = remove_result {
        return Err(error.context(format!(
            "cannot restore the previous installation while {} still exists; backup kept at {}",
            target.display(),
            backup.display()
        )));
    }
    restore_backup(target, backup)
}

fn with_rollback(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!("rollback failed: {rollback_error:#}")),
    }
}

fn ensure_directory_chain(path: &Path) -> Result<()> {
    let ancestors: Vec<&Path> = path
        .ancestors()
        .filter(|ancestor| ancestor.has_root())
        .collect();
    for ancestor in ancestors.into_iter().rev() {
        match winutil::attributes_if_exists(ancestor)? {
            Some(attributes) => {
                if !winutil::is_directory(attributes) {
                    bail!("install parent is not a directory: {}", ancestor.display());
                }
                if winutil::is_reparse_point(attributes) {
                    bail!("install parent is a reparse point: {}", ancestor.display());
                }
            }
            None => match std::fs::create_dir(ancestor) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    winutil::require_directory(ancestor, "install parent")?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create install parent {}", ancestor.display())
                    });
                }
            },
        }
    }
    Ok(())
}

fn create_stage_directory(parent: &Path) -> Result<PathBuf> {
    for _ in 0..MAX_STAGE_ATTEMPTS {
        let stage = unique_path(parent, ".aip-stage-")?;
        if stage.to_string_lossy().encode_utf16().count() > path::MAX_INSTALL_PATH_UTF16 {
            bail!("staging path is too long");
        }
        match acl::create_private_directory(&stage) {
            Ok(true) => return Ok(stage),
            Ok(false) => continue,
            Err(error) => return Err(error),
        }
    }
    bail!("could not allocate a unique staging directory");
}

fn unique_path(parent: &Path, prefix: &str) -> Result<PathBuf> {
    let random = winutil::random_hex::<16>()?;
    let path = parent.join(format!("{prefix}{random}"));
    if path.to_string_lossy().encode_utf16().count() > path::MAX_INSTALL_PATH_UTF16 {
        bail!("temporary path is too long");
    }
    Ok(path)
}

fn reset_dacl_tree(root: &Path) -> Result<()> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        acl::enable_dacl_inheritance(&directory)?;
        for entry in std::fs::read_dir(&directory).with_context(|| {
            format!("failed to read installed directory {}", directory.display())
        })? {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_attributes() & winutil::FILE_ATTRIBUTE_REPARSE_POINT_BITS != 0 {
                bail!(
                    "installed tree contains a reparse point: {}",
                    path.display()
                );
            }
            if metadata.is_dir() {
                directories.push(path);
            } else {
                acl::enable_dacl_inheritance(&path)?;
            }
        }
    }
    Ok(())
}
fn prepare_shortcut_icon(
    package_root: &Path,
    configured_icon: Option<&str>,
    stage: &Path,
) -> Option<PathBuf> {
    let configured_icon = configured_icon?;
    let relative = match path::validate_package_relative(configured_icon) {
        Ok(relative) => relative,
        Err(error) => {
            log::line(format!(
                "backend: cannot use configured shortcut icon {configured_icon:?}: {error:#}"
            ));
            return None;
        }
    };
    let source = package_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let result = (|| -> Result<PathBuf> {
        winutil::require_regular_file(&source, "shortcut icon")?;
        winutil::ensure_no_reparse_components(&source)?;
        let canonical_root = std::fs::canonicalize(package_root)?;
        let canonical_source = std::fs::canonicalize(&source)?;
        if !canonical_source.starts_with(&canonical_root) {
            bail!("shortcut icon escapes the package root");
        }
        let metadata = std::fs::metadata(&source)?;
        if metadata.len() == 0 {
            bail!("shortcut icon is empty");
        }
        if metadata.len() > MAX_ICON_BYTES {
            bail!("shortcut icon is larger than {MAX_ICON_BYTES} bytes");
        }
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.is_empty() && extension.len() <= 8)
            .unwrap_or("ico");
        let destination = stage.join(format!(
            ".aip-icon-{}.{}",
            winutil::random_hex::<8>()?,
            extension
        ));
        let copy_result = (|| -> Result<()> {
            let mut input = std::fs::File::open(&source)?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            Ok(())
        })();
        if let Err(error) = copy_result {
            let _ = std::fs::remove_file(&destination);
            return Err(error);
        }
        Ok(PathBuf::from(
            destination
                .strip_prefix(stage)?
                .to_string_lossy()
                .replace('\\', "/"),
        ))
    })();
    match result {
        Ok(path) => Some(path),
        Err(error) => {
            log::line(format!(
                "backend: cannot stage configured shortcut icon {}: {error:#}",
                source.display()
            ));
            None
        }
    }
}
fn verify_installed_target(target: &Path, install_location: &Path) -> Result<()> {
    winutil::require_regular_file(target, "shortcut target")?;
    winutil::ensure_no_reparse_components(target)?;
    let canonical_target = std::fs::canonicalize(target)?;
    let canonical_root = std::fs::canonicalize(install_location)?;
    if !canonical_target.starts_with(&canonical_root) {
        bail!("selected shortcut target escaped the install directory");
    }
    Ok(())
}

pub fn select_launch_target(executables: &[PathBuf], name: &str) -> Result<PathBuf> {
    if executables.is_empty() {
        bail!("payload does not contain an executable to use as the shortcut target");
    }
    let wanted = path::sanitize_file_name(name).to_lowercase();
    let exact = executables
        .iter()
        .filter(|candidate| {
            candidate
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.to_lowercase() == wanted)
        })
        .cloned()
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    let root_executables = executables
        .iter()
        .filter(|candidate| candidate.components().count() == 1)
        .cloned()
        .collect::<Vec<_>>();
    if root_executables.len() == 1 {
        return Ok(root_executables[0].clone());
    }
    if executables.len() == 1 {
        return Ok(executables[0].clone());
    }
    let names = executables
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "payload contains multiple executables and none uniquely matches the application name; candidates: {names}"
    )
}

fn remove_tree(path: &Path) -> Result<()> {
    let mut stack = vec![(path.to_path_buf(), false)];
    while let Some((current, visited)) = stack.pop() {
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        };
        let is_reparse =
            metadata.file_attributes() & winutil::FILE_ATTRIBUTE_REPARSE_POINT_BITS != 0;
        if metadata.is_dir() && !is_reparse {
            if visited {
                let _ = winutil::clear_readonly(&current);
                std::fs::remove_dir(&current)
                    .with_context(|| format!("failed to remove directory {}", current.display()))?;
            } else {
                stack.push((current.clone(), true));
                for entry in std::fs::read_dir(&current)
                    .with_context(|| format!("failed to read directory {}", current.display()))?
                {
                    stack.push((entry?.path(), false));
                }
            }
        } else {
            let _ = winutil::clear_readonly(&current);
            if metadata.is_dir() {
                std::fs::remove_dir(&current)
                    .or_else(|_| std::fs::remove_file(&current))
                    .with_context(|| {
                        format!("failed to remove reparse point {}", current.display())
                    })?;
            } else {
                std::fs::remove_file(&current)
                    .with_context(|| format!("failed to remove file {}", current.display()))?;
            }
        }
    }
    Ok(())
}
struct PathGuard {
    path: PathBuf,
    active: bool,
}

impl PathGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    fn mark_moved(&mut self) {
        self.active = false;
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = remove_tree(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Defaults;
    use std::io::Write;
    use zip::CompressionMethod;

    fn write_payload(root: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = root.join("payload.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            writer
                .start_file(
                    *name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    fn config(name: &str) -> Config {
        Config {
            protocol: 1,
            name: name.to_string(),
            icon: None,
            background: None,
            defaults: Defaults {
                install_location: r"%LOCALAPPDATA%\Programs\Test".into(),
                create_start_menu_folder: false,
                create_desktop_shortcut: false,
            },
        }
    }

    #[test]
    fn installs_payload_transactionally() {
        let package = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let payload = write_payload(
            package.path(),
            &[("Example.exe", b"exe"), ("data/config.json", b"{}")],
        );
        let target = destination.path().join("apps").join("Example");
        let settings = InstallSettings {
            install_location: target.to_string_lossy().into_owned(),
            create_start_menu_folder: false,
            create_desktop_shortcut: false,
        };
        let outcome = install(package.path(), &payload, &config("Example"), &settings).unwrap();
        assert_eq!(outcome.files, 2);
        assert_eq!(std::fs::read(target.join("Example.exe")).unwrap(), b"exe");
        assert_eq!(
            std::fs::read(target.join("data/config.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn replaces_an_existing_installation() {
        let package = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let target = destination.path().join("apps").join("Example");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("old.txt"), b"old").unwrap();
        let payload = write_payload(package.path(), &[("Example.exe", b"new")]);
        let settings = InstallSettings {
            install_location: target.to_string_lossy().into_owned(),
            create_start_menu_folder: false,
            create_desktop_shortcut: false,
        };
        install(package.path(), &payload, &config("Example"), &settings).unwrap();
        assert!(!target.join("old.txt").exists());
        assert_eq!(std::fs::read(target.join("Example.exe")).unwrap(), b"new");
    }

    #[test]
    fn cleans_stage_when_extraction_fails() {
        let package = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let payload = write_payload(package.path(), &[("../escape.txt", b"bad")]);
        let parent = destination.path().join("apps");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("Example");
        let settings = InstallSettings {
            install_location: target.to_string_lossy().into_owned(),
            create_start_menu_folder: false,
            create_desktop_shortcut: false,
        };
        assert!(install(package.path(), &payload, &config("Example"), &settings).is_err());
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
    }

    #[test]
    fn stages_a_configured_shortcut_icon() {
        let package = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        let assets = package.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("icon.ico"), b"icon-bytes").unwrap();

        let relative = prepare_shortcut_icon(package.path(), Some("assets/icon.ico"), stage.path())
            .expect("icon should be staged");
        assert!(relative.to_string_lossy().starts_with(".aip-icon-"));
        assert_eq!(
            std::fs::read(stage.path().join(&relative)).unwrap(),
            b"icon-bytes"
        );
    }

    #[test]
    fn invalid_shortcut_icon_falls_back_to_executable() {
        let package = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        assert!(
            prepare_shortcut_icon(package.path(), Some("C:\\outside.ico"), stage.path()).is_none()
        );
        assert!(std::fs::read_dir(stage.path()).unwrap().next().is_none());
    }

    #[test]
    fn chooses_deterministic_launch_targets() {
        let one = vec![PathBuf::from("bin/Example.exe")];
        assert_eq!(
            select_launch_target(&one, "Example").unwrap(),
            PathBuf::from("bin/Example.exe")
        );
        let root_one = vec![PathBuf::from("Tool.exe"), PathBuf::from("bin/Helper.exe")];
        assert_eq!(
            select_launch_target(&root_one, "App").unwrap(),
            PathBuf::from("Tool.exe")
        );
        let named = vec![PathBuf::from("bin/App.exe"), PathBuf::from("Tool.exe")];
        assert_eq!(
            select_launch_target(&named, "App").unwrap(),
            PathBuf::from("bin/App.exe")
        );
        assert!(select_launch_target(&[], "App").is_err());
        assert!(
            select_launch_target(&[PathBuf::from("A.exe"), PathBuf::from("B.exe")], "App").is_err()
        );
    }

    #[test]
    fn removes_a_deep_tree_without_recursion() {
        let root = tempfile::tempdir().unwrap();
        let mut current = root.path().to_path_buf();
        for _ in 0..512 {
            current.push("d");
            std::fs::create_dir(&current).unwrap();
        }
        std::fs::write(current.join("file"), b"x").unwrap();
        remove_tree(root.path()).unwrap();
        assert!(!root.path().exists());
    }

    #[test]
    fn uncommitted_guard_removes_stage() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("file"), b"x").unwrap();
        {
            let _guard = PathGuard::new(stage.clone());
        }
        assert!(!stage.exists());
    }
}
