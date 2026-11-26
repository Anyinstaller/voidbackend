use anyhow::{bail, Context, Result};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::log;
use crate::path;
use crate::winutil;

const SHELL_LINK_CLSID: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);

#[derive(Debug, Clone)]
pub struct ShortcutSpec {
    pub name: String,
    pub target: PathBuf,
    pub icon: PathBuf,
    pub working_directory: PathBuf,
    pub create_desktop_shortcut: bool,
    pub create_start_menu_folder: bool,
}

#[derive(Debug)]
struct Placement {
    final_path: PathBuf,
    temp_path: Option<PathBuf>,
    backup_path: Option<PathBuf>,
    installed: bool,
}

/// Create the requested shortcuts transactionally. Existing link files are
/// moved aside while temporary links are promoted, then restored if any step
/// fails.
pub fn create_shortcuts(spec: &ShortcutSpec) -> Result<()> {
    if !spec.create_desktop_shortcut && !spec.create_start_menu_folder {
        return Ok(());
    }

    validate_target(&spec.target, &spec.icon, &spec.working_directory)?;
    let shortcut_name = path::sanitize_file_name(&spec.name);
    let link_name = format!("{shortcut_name}.lnk");

    let desktop_folder = if spec.create_desktop_shortcut {
        let folder =
            winutil::known_folder(&FOLDERID_Desktop).context("desktop folder unavailable")?;
        winutil::require_directory(&folder, "desktop folder")?;
        winutil::ensure_no_reparse_components(&folder)?;
        Some(folder)
    } else {
        None
    };

    let programs_folder = if spec.create_start_menu_folder {
        let folder =
            winutil::known_folder(&FOLDERID_Programs).context("Start Menu folder unavailable")?;
        winutil::require_directory(&folder, "Start Menu folder")?;
        winutil::ensure_no_reparse_components(&folder)?;
        Some(folder)
    } else {
        None
    };

    let mut created_start_menu_folder = None;
    let start_menu_folder = match programs_folder {
        Some(folder) => {
            let app_folder = folder.join(&shortcut_name);
            match winutil::attributes_if_exists(&app_folder)? {
                Some(attributes) => {
                    if !winutil::is_directory(attributes) {
                        bail!(
                            "Start Menu shortcut path is not a directory: {}",
                            app_folder.display()
                        );
                    }
                    if winutil::is_reparse_point(attributes) {
                        bail!("Start Menu shortcut directory is a reparse point");
                    }
                }
                None => {
                    std::fs::create_dir(&app_folder).with_context(|| {
                        format!(
                            "failed to create Start Menu folder {}",
                            app_folder.display()
                        )
                    })?;
                    created_start_menu_folder = Some(app_folder.clone());
                }
            }
            Some(app_folder)
        }
        None => None,
    };

    let mut placements = Vec::new();
    if let Some(folder) = start_menu_folder {
        placements.push(Placement::new(folder.join(&link_name)));
    }
    if let Some(folder) = desktop_folder {
        placements.push(Placement::new(folder.join(&link_name)));
    }

    let result = (|| -> Result<()> {
        let _com = ComGuard::new()?;
        for placement in &mut placements {
            let temp = temporary_link_path(&placement.final_path)?;
            create_link(
                &temp,
                &spec.name,
                &spec.target,
                &spec.icon,
                &spec.working_directory,
            )?;
            placement.temp_path = Some(temp);
        }
        promote_links(&mut placements)
    })();

    if let Err(error) = result {
        let rollback_errors = rollback_placements(&placements);
        if let Some(folder) = created_start_menu_folder {
            let _ = std::fs::remove_dir(folder);
        }
        if rollback_errors.is_empty() {
            return Err(error);
        }
        return Err(error.context(format!(
            "shortcut rollback also failed: {}",
            rollback_errors.join("; ")
        )));
    }

    for placement in &placements {
        if let Some(backup) = &placement.backup_path {
            if let Err(error) = std::fs::remove_file(backup) {
                log::line(format!(
                    "backend: could not remove shortcut backup {}: {error}",
                    backup.display()
                ));
            }
        }
    }
    Ok(())
}

impl Placement {
    fn new(final_path: PathBuf) -> Self {
        Self {
            final_path,
            temp_path: None,
            backup_path: None,
            installed: false,
        }
    }
}

fn promote_links(placements: &mut [Placement]) -> Result<()> {
    for placement in placements {
        let temp = placement
            .temp_path
            .as_ref()
            .context("temporary shortcut is missing")?
            .clone();
        match std::fs::symlink_metadata(&placement.final_path) {
            Ok(metadata) => {
                if !metadata.is_file()
                    || metadata.file_attributes() & winutil::FILE_ATTRIBUTE_REPARSE_POINT_BITS != 0
                {
                    bail!("existing shortcut is not a regular file");
                }
                let backup = backup_link_path(&placement.final_path)?;
                std::fs::rename(&placement.final_path, &backup).with_context(|| {
                    format!(
                        "failed to move existing shortcut {} aside",
                        placement.final_path.display()
                    )
                })?;
                placement.backup_path = Some(backup);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect existing shortcut {}",
                        placement.final_path.display()
                    )
                });
            }
        }
        std::fs::rename(&temp, &placement.final_path).with_context(|| {
            format!(
                "failed to install shortcut {}",
                placement.final_path.display()
            )
        })?;
        placement.temp_path = None;
        placement.installed = true;
    }
    Ok(())
}

fn rollback_placements(placements: &[Placement]) -> Vec<String> {
    let mut errors = Vec::new();
    for placement in placements.iter().rev() {
        if placement.installed {
            if let Err(error) = std::fs::remove_file(&placement.final_path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    errors.push(format!(
                        "remove {}: {error}",
                        placement.final_path.display()
                    ));
                }
            }
        }
        if let Some(backup) = &placement.backup_path {
            if let Err(error) = std::fs::rename(backup, &placement.final_path) {
                errors.push(format!(
                    "restore {}: {error}",
                    placement.final_path.display()
                ));
            }
        }
        if let Some(temp) = &placement.temp_path {
            if let Err(error) = std::fs::remove_file(temp) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    errors.push(format!("remove {}: {error}", temp.display()));
                }
            }
        }
    }
    errors
}

fn validate_target(target: &Path, icon: &Path, working_directory: &Path) -> Result<()> {
    winutil::require_regular_file(target, "shortcut target")?;
    winutil::require_regular_file(icon, "shortcut icon")?;
    winutil::require_directory(working_directory, "shortcut working directory")?;
    winutil::ensure_no_reparse_components(target)?;
    winutil::ensure_no_reparse_components(icon)?;
    let canonical_target = std::fs::canonicalize(target)
        .with_context(|| format!("failed to resolve shortcut target {}", target.display()))?;
    let canonical_icon = std::fs::canonicalize(icon)
        .with_context(|| format!("failed to resolve shortcut icon {}", icon.display()))?;
    let canonical_working = std::fs::canonicalize(working_directory).with_context(|| {
        format!(
            "failed to resolve shortcut working directory {}",
            working_directory.display()
        )
    })?;
    if !canonical_target.starts_with(&canonical_working) {
        bail!("shortcut target must be inside the installation directory");
    }
    if !canonical_icon.starts_with(&canonical_working) {
        bail!("shortcut icon must be inside the installation directory");
    }
    Ok(())
}

fn temporary_link_path(final_path: &Path) -> Result<PathBuf> {
    let parent = final_path
        .parent()
        .context("shortcut destination has no parent")?;
    let random = winutil::random_hex::<8>()?;
    Ok(parent.join(format!(".aip-link-{random}.tmp.lnk")))
}

fn backup_link_path(final_path: &Path) -> Result<PathBuf> {
    let parent = final_path
        .parent()
        .context("shortcut destination has no parent")?;
    let random = winutil::random_hex::<8>()?;
    Ok(parent.join(format!(".aip-link-{random}.backup.lnk")))
}

fn create_link(
    link_path: &Path,
    name: &str,
    target: &Path,
    icon: &Path,
    working_directory: &Path,
) -> Result<()> {
    let link: IShellLinkW = unsafe {
        CoCreateInstance(
            &SHELL_LINK_CLSID,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
        .context("failed to create IShellLinkW")?
    };

    let target_wide = winutil::wide(target);
    let working_wide = winutil::wide(working_directory);
    let name_wide = winutil::wide_str(name);
    let empty_wide = winutil::wide_str("");
    let icon_wide = winutil::wide(icon);
    unsafe {
        link.SetPath(PCWSTR(target_wide.as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(working_wide.as_ptr()))?;
        link.SetArguments(PCWSTR(empty_wide.as_ptr()))?;
        link.SetDescription(PCWSTR(name_wide.as_ptr()))?;
        link.SetIconLocation(PCWSTR(icon_wide.as_ptr()), 0)?;
        link.SetShowCmd(SW_SHOWNORMAL)?;
    }

    let persist: IPersistFile = link.cast().context("failed to obtain IPersistFile")?;
    let link_wide = winutil::wide(link_path);
    unsafe {
        persist
            .Save(PCWSTR(link_wide.as_ptr()), true)
            .with_context(|| format!("failed to save shortcut {}", link_path.display()))?;
    }
    Ok(())
}

struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        result.ok().context("CoInitializeEx failed")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_shell_link_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Example.exe");
        std::fs::write(&target, b"not a real executable").unwrap();
        let working = dir.path();
        let link = dir.path().join("Example.lnk");
        let _com = ComGuard::new().unwrap();
        create_link(&link, "Example", &target, &target, working).unwrap();
        let metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(metadata.is_file());
        assert!(metadata.len() > 0);
    }
}
