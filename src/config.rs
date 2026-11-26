use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use toml::Value;

use crate::path;
use crate::winutil;

pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
pub const MAX_NAME_CHARS: usize = 128;
pub const PROTOCOL_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub protocol: i64,
    pub name: String,
    pub icon: Option<String>,
    pub background: Option<String>,
    pub defaults: Defaults,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub install_location: String,
    pub create_start_menu_folder: bool,
    pub create_desktop_shortcut: bool,
}

impl Config {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CONFIG_BYTES {
            bail!("config.toml is larger than 64 KiB");
        }
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            bail!("config.toml must be UTF-8 without a BOM");
        }
        let text = std::str::from_utf8(bytes).context("config.toml is not valid UTF-8")?;
        let value: Value = toml::from_str(text).context("config.toml is not valid TOML")?;
        let table = value
            .as_table()
            .context("config.toml root must be a TOML table")?;

        let protocol = match table.get("protocol") {
            Some(Value::Integer(value)) => *value,
            Some(_) => bail!("protocol must be an integer"),
            None => bail!("protocol is required"),
        };
        if protocol != PROTOCOL_VERSION {
            bail!("unsupported AIP protocol number: {protocol}");
        }

        let name = match table.get("name") {
            Some(Value::String(value)) => value.clone(),
            Some(_) => bail!("name must be a string"),
            None => bail!("name is required"),
        };
        validate_name(&name)?;

        let icon = optional_resource_path(table.get("icon"), "icon", false)?;
        let background = optional_resource_path(table.get("background"), "background", true)?;

        let defaults_table = match table.get("defaults") {
            Some(Value::Table(value)) => value,
            Some(_) => bail!("defaults must be a table"),
            None => bail!("[defaults] is required"),
        };

        let install_location = match defaults_table.get("installLocation") {
            Some(Value::String(value)) => value.clone(),
            Some(_) => bail!("defaults.installLocation must be a string"),
            None => bail!("defaults.installLocation is required"),
        };
        path::validate_install_location(&install_location, None)
            .context("defaults.installLocation is invalid")?;

        let create_start_menu_folder = match defaults_table.get("createStartMenuFolder") {
            Some(Value::Boolean(value)) => *value,
            Some(_) => bail!("defaults.createStartMenuFolder must be a boolean"),
            None => bail!("defaults.createStartMenuFolder is required"),
        };
        let create_desktop_shortcut = match defaults_table.get("createDesktopShortcut") {
            Some(Value::Boolean(value)) => *value,
            Some(_) => bail!("defaults.createDesktopShortcut must be a boolean"),
            None => bail!("defaults.createDesktopShortcut is required"),
        };

        Ok(Self {
            protocol,
            name,
            icon,
            background,
            defaults: Defaults {
                install_location,
                create_start_menu_folder,
                create_desktop_shortcut,
            },
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        winutil::require_regular_file(path, "config.toml")?;
        winutil::ensure_no_reparse_components(path)?;
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.len() > MAX_CONFIG_BYTES as u64 {
            bail!("config.toml is larger than 64 KiB");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if bytes.len() > MAX_CONFIG_BYTES {
            bail!("config.toml is larger than 64 KiB");
        }
        Self::parse(&bytes)
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("name must not be empty");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        bail!("name must be at most {MAX_NAME_CHARS} characters");
    }
    if name.chars().any(char::is_control) {
        bail!("name must not contain control characters");
    }
    Ok(())
}

fn optional_resource_path(
    value: Option<&Value>,
    field: &str,
    background: bool,
) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = match value {
        Value::String(value) => value,
        _ => bail!("{field} must be a string"),
    };
    if text.is_empty() {
        return Ok(None);
    }
    if text.contains('\0') {
        bail!("{field} must not contain NUL");
    }
    if background && (path::is_special_background(text) || path::is_hex_color(text)) {
        return Ok(Some(text.clone()));
    }
    let normalized = path::validate_package_relative(text)
        .with_context(|| format!("{field} must be a package-internal relative path"))?;
    Ok(Some(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
protocol = 1
name = "Example App"
icon = "assets/icon.ico"
background = "assets/background.avif"

[defaults]
installLocation = '%LocalAppData%\Programs\Example App'
createStartMenuFolder = true
createDesktopShortcut = false
"#;

    #[test]
    fn parses_valid_config() {
        let config = Config::parse(VALID.as_bytes()).unwrap();
        assert_eq!(config.protocol, 1);
        assert_eq!(config.name, "Example App");
        assert_eq!(config.icon.as_deref(), Some("assets/icon.ico"));
        assert_eq!(config.background.as_deref(), Some("assets/background.avif"));
        assert!(config.defaults.create_start_menu_folder);
        assert!(!config.defaults.create_desktop_shortcut);
    }

    #[test]
    fn accepts_effects_and_empty_optional_fields() {
        let config = Config::parse(
            br#"
protocol = 1
name = "X"
icon = ""
background = "acrylic"

[defaults]
installLocation = "C:\\Apps\\X"
createStartMenuFolder = true
createDesktopShortcut = true
"#,
        )
        .unwrap();
        assert_eq!(config.icon, None);
        assert_eq!(config.background.as_deref(), Some("acrylic"));
    }

    #[test]
    fn rejects_bad_configs() {
        let cases = [
            ("protocol = 2\nname = \"X\"\n[defaults]\ninstallLocation = 'C:\\X'\ncreateStartMenuFolder = true\ncreateDesktopShortcut = true", "protocol"),
            ("protocol = 1\n[defaults]\ninstallLocation = 'C:\\X'\ncreateStartMenuFolder = true\ncreateDesktopShortcut = true", "name"),
            ("protocol = 1\nname = \"\"\n[defaults]\ninstallLocation = 'C:\\X'\ncreateStartMenuFolder = true\ncreateDesktopShortcut = true", "name"),
            ("protocol = 1\nname = \"X\"\nicon = 'C:\\outside.ico'\n[defaults]\ninstallLocation = 'C:\\X'\ncreateStartMenuFolder = true\ncreateDesktopShortcut = true", "icon"),
            ("protocol = 1\nname = \"X\"\nbackground = 'C:\\outside.avif'\n[defaults]\ninstallLocation = 'C:\\X'\ncreateStartMenuFolder = true\ncreateDesktopShortcut = true", "background"),
            ("protocol = 1\nname = \"X\"\n[defaults]\ninstallLocation = '\\\\server\\share\\X'\ncreateStartMenuFolder = true\ncreateDesktopShortcut = true", "install"),
            ("protocol = 1\nname = \"X\"\n[defaults]\ninstallLocation = 'C:\\X'\ncreateStartMenuFolder = \"yes\"\ncreateDesktopShortcut = true", "StartMenu"),
        ];
        for (text, label) in cases {
            assert!(
                Config::parse(text.as_bytes()).is_err(),
                "expected rejection for {label}"
            );
        }
    }

    #[test]
    fn rejects_bom_and_oversize() {
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(VALID.as_bytes());
        assert!(Config::parse(&bom).is_err());
        assert!(Config::parse(&vec![b' '; MAX_CONFIG_BYTES + 1]).is_err());
    }
}
