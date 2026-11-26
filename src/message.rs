use anyhow::{Context, Result};
use std::path::Path;
use toml::{Table, Value};

use crate::config::Defaults;
use crate::frame::Frame;
use crate::path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting<T> {
    Valid(T),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSettings {
    pub install_location: String,
    pub create_start_menu_folder: bool,
    pub create_desktop_shortcut: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    Ignore,
    Install(Result<InstallSettings, String>),
}

#[derive(Debug)]
pub struct SessionState {
    install_location: Setting<String>,
    create_start_menu_folder: Setting<bool>,
    create_desktop_shortcut: Setting<bool>,
    started: bool,
    finished: bool,
}

impl SessionState {
    pub fn new(defaults: &Defaults, session_root: &Path) -> Result<Self> {
        let install_location =
            path::validate_install_location(&defaults.install_location, Some(session_root))
                .context("default install location is invalid")?;
        Ok(Self {
            install_location: Setting::Valid(install_location),
            create_start_menu_folder: Setting::Valid(defaults.create_start_menu_folder),
            create_desktop_shortcut: Setting::Valid(defaults.create_desktop_shortcut),
            started: false,
            finished: false,
        })
    }

    pub fn apply(&mut self, frame: &Frame, session_root: &Path) -> SessionAction {
        if self.finished {
            return SessionAction::Ignore;
        }

        match frame.message_type.as_str() {
            "setInstallLocation" => {
                if !self.started {
                    self.install_location =
                        match frame.document.get("value").and_then(Value::as_str) {
                            Some(value) => {
                                match path::validate_install_location(value, Some(session_root)) {
                                    Ok(value) => Setting::Valid(value),
                                    Err(error) => Setting::Invalid(format!("{error:#}")),
                                }
                            }
                            None => Setting::Invalid(
                                "setInstallLocation requires a string value".to_string(),
                            ),
                        };
                }
                SessionAction::Ignore
            }
            "setCreateStartMenuFolder" => {
                if !self.started {
                    self.create_start_menu_folder =
                        parse_bool_setting(&frame.document, "setCreateStartMenuFolder");
                }
                SessionAction::Ignore
            }
            "setCreateDesktopShortcut" => {
                if !self.started {
                    self.create_desktop_shortcut =
                        parse_bool_setting(&frame.document, "setCreateDesktopShortcut");
                }
                SessionAction::Ignore
            }
            "startInstall" if !self.started => {
                self.started = true;
                SessionAction::Install(self.settings())
            }
            _ => SessionAction::Ignore,
        }
    }

    pub fn mark_finished(&mut self) {
        self.finished = true;
    }

    fn settings(&self) -> Result<InstallSettings, String> {
        let mut errors = Vec::new();
        let install_location = match &self.install_location {
            Setting::Valid(value) => Some(value.clone()),
            Setting::Invalid(error) => {
                errors.push(format!("install location: {error}"));
                None
            }
        };
        let create_start_menu_folder = match self.create_start_menu_folder {
            Setting::Valid(value) => Some(value),
            Setting::Invalid(ref error) => {
                errors.push(format!("createStartMenuFolder: {error}"));
                None
            }
        };
        let create_desktop_shortcut = match self.create_desktop_shortcut {
            Setting::Valid(value) => Some(value),
            Setting::Invalid(ref error) => {
                errors.push(format!("createDesktopShortcut: {error}"));
                None
            }
        };
        if !errors.is_empty() {
            return Err(format!(
                "invalid installation settings: {}",
                errors.join("; ")
            ));
        }
        Ok(InstallSettings {
            install_location: install_location.expect("checked above"),
            create_start_menu_folder: create_start_menu_folder.expect("checked above"),
            create_desktop_shortcut: create_desktop_shortcut.expect("checked above"),
        })
    }
}

fn parse_bool_setting(document: &Table, message_type: &str) -> Setting<bool> {
    match document.get("value").and_then(Value::as_bool) {
        Some(value) => Setting::Valid(value),
        None => Setting::Invalid(format!("{message_type} requires a boolean value")),
    }
}

pub fn install_progress(
    stage: &str,
    percent: u8,
    current: Option<u64>,
    total: Option<u64>,
) -> Table {
    let mut table = Table::new();
    table.insert(
        "type".to_string(),
        Value::String("installProgress".to_string()),
    );
    table.insert("stage".to_string(), Value::String(stage.to_string()));
    table.insert(
        "percent".to_string(),
        Value::Integer(percent.min(100) as i64),
    );
    if let Some(current) = current {
        table.insert(
            "current".to_string(),
            Value::Integer(current.min(i64::MAX as u64) as i64),
        );
    }
    if let Some(total) = total {
        table.insert(
            "total".to_string(),
            Value::Integer(total.min(i64::MAX as u64) as i64),
        );
    }
    table
}

pub fn install_finished(success: bool, error: Option<&str>) -> Table {
    let mut table = Table::new();
    table.insert(
        "type".to_string(),
        Value::String("installFinished".to_string()),
    );
    table.insert("success".to_string(), Value::Boolean(success));
    if let Some(error) = error {
        table.insert("error".to_string(), Value::String(sanitize_error(error)));
    }
    table
}

pub fn sanitize_error(error: &str) -> String {
    let mut value: String = error
        .chars()
        .filter(|character| *character != '\0')
        .take(4096)
        .collect();
    if value.is_empty() {
        value.push_str("installation failed");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Defaults;
    use crate::frame;

    fn defaults() -> Defaults {
        Defaults {
            install_location: r"%LOCALAPPDATA%\Programs\Example".to_string(),
            create_start_menu_folder: true,
            create_desktop_shortcut: false,
        }
    }

    fn frame(body: &str) -> Frame {
        frame::parse_message(body.as_bytes()).unwrap()
    }

    fn test_install_location(name: &str) -> String {
        let drive = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .chars()
            .next()
            .unwrap();
        format!(r"{drive}:\Apps\{name}")
    }

    #[test]
    fn last_valid_setting_wins() {
        let root = std::env::current_dir().unwrap();
        let mut state = SessionState::new(&defaults(), &root).unwrap();
        let first = test_install_location("First");
        let second = test_install_location("Second");
        state.apply(
            &frame(&format!("type = \"setInstallLocation\"\nvalue = '{first}'")),
            &root,
        );
        state.apply(
            &frame(&format!(
                "type = \"setInstallLocation\"\nvalue = '{second}'"
            )),
            &root,
        );
        let SessionAction::Install(Ok(settings)) =
            state.apply(&frame("type = \"startInstall\""), &root)
        else {
            panic!("expected installation settings");
        };
        assert_eq!(settings.install_location, second);
        assert!(settings.create_start_menu_folder);
        assert!(!settings.create_desktop_shortcut);
    }

    #[test]
    fn invalid_setting_is_cleared_by_later_valid_value() {
        let root = std::env::current_dir().unwrap();
        let mut state = SessionState::new(&defaults(), &root).unwrap();
        state.apply(
            &frame("type = \"setCreateDesktopShortcut\"\nvalue = \"yes\""),
            &root,
        );
        state.apply(
            &frame("type = \"setCreateDesktopShortcut\"\nvalue = true"),
            &root,
        );
        assert!(matches!(
            state.apply(&frame("type = \"startInstall\""), &root),
            SessionAction::Install(Ok(_))
        ));
    }

    #[test]
    fn invalid_setting_fails_start_install() {
        let root = std::env::current_dir().unwrap();
        let mut state = SessionState::new(&defaults(), &root).unwrap();
        state.apply(
            &frame("type = \"setInstallLocation\"\nvalue = \"C:relative\""),
            &root,
        );
        let SessionAction::Install(Err(error)) =
            state.apply(&frame("type = \"startInstall\""), &root)
        else {
            panic!("expected failure");
        };
        assert!(error.contains("install location"));
    }

    #[test]
    fn only_first_start_install_takes_effect() {
        let root = std::env::current_dir().unwrap();
        let mut state = SessionState::new(&defaults(), &root).unwrap();
        assert!(matches!(
            state.apply(&frame("type = \"startInstall\""), &root),
            SessionAction::Install(Ok(_))
        ));
        assert_eq!(
            state.apply(&frame("type = \"startInstall\""), &root),
            SessionAction::Ignore
        );
    }

    #[test]
    fn ignores_unknown_and_backend_directed_messages() {
        let root = std::env::current_dir().unwrap();
        let mut state = SessionState::new(&defaults(), &root).unwrap();
        assert_eq!(
            state.apply(&frame("type = \"readyToExit\""), &root),
            SessionAction::Ignore
        );
        assert_eq!(
            state.apply(&frame("type = \"unknownMessage\"\nvalue = 1"), &root),
            SessionAction::Ignore
        );
    }

    #[test]
    fn formats_progress_message() {
        let table = install_progress("extracting", 42, Some(512), Some(1024));
        assert_eq!(table["type"].as_str(), Some("installProgress"));
        assert_eq!(table["stage"].as_str(), Some("extracting"));
        assert_eq!(table["percent"].as_integer(), Some(42));
        assert_eq!(table["current"].as_integer(), Some(512));
        assert_eq!(table["total"].as_integer(), Some(1024));
    }

    #[test]
    fn formats_failure_message() {
        let table = install_finished(false, Some("bad\0error"));
        assert_eq!(table["type"].as_str(), Some("installFinished"));
        assert_eq!(table["success"].as_bool(), Some(false));
        assert_eq!(table["error"].as_str(), Some("baderror"));
    }
}
