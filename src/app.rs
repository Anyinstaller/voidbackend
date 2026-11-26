use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use crate::cli;
use crate::config::Config;
use crate::install;
use crate::log;
use crate::message::{self, SessionAction, SessionState};
use crate::pipe::PipeClient;
use crate::winutil;

/// Run one AIP v1 backend session.
pub fn run<I>(arguments: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .context("SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) failed")?;
    let pipe_name = cli::parse_pipe_name(arguments)?;
    let package_root = std::env::current_dir().context("failed to read current directory")?;
    validate_package_root(&package_root)?;

    let config_path = package_root.join("config.toml");
    let payload_path = package_root.join("payload.zip");
    winutil::require_regular_file(&config_path, "config.toml")?;
    winutil::require_regular_file(&payload_path, "payload.zip")?;
    let config = Config::read(&config_path)?;
    let mut state = SessionState::new(&config.defaults, &package_root)?;

    let pipe = PipeClient::connect(&pipe_name)?;
    loop {
        let frame = pipe.read_frame()?;
        match state.apply(&frame, &package_root) {
            SessionAction::Ignore => continue,
            SessionAction::Install(settings) => {
                let response = match settings {
                    Ok(settings) => {
                        let mut progress_write_failed = false;
                        let result = install::install_with_progress(
                            &package_root,
                            &payload_path,
                            &config,
                            &settings,
                            |progress| {
                                if progress_write_failed {
                                    return;
                                }
                                let table = message::install_progress(
                                    progress.stage.as_str(),
                                    progress.percent,
                                    progress.current,
                                    progress.total,
                                );
                                if let Err(error) = pipe.write_table(&table) {
                                    progress_write_failed = true;
                                    log::line(format!(
                                        "backend: failed to send installProgress: {error:#}"
                                    ));
                                }
                            },
                        );
                        match result {
                            Ok(outcome) => {
                                log::line(format!(
                                    "backend: installed {} file(s) to {}",
                                    outcome.files,
                                    outcome.install_location.display()
                                ));
                                message::install_finished(true, None)
                            }
                            Err(error) => {
                                let text = format!("{error:#}");
                                log::line(format!("backend: installation failed: {text}"));
                                message::install_finished(false, Some(&text))
                            }
                        }
                    }
                    Err(error) => {
                        log::line(format!("backend: invalid installation settings: {error}"));
                        message::install_finished(false, Some(&error))
                    }
                };
                pipe.write_table(&response)?;
                state.mark_finished();
                return Ok(());
            }
        }
    }
}

fn validate_package_root(package_root: &Path) -> Result<()> {
    winutil::require_directory(package_root, "package root")?;
    winutil::ensure_no_reparse_components(package_root)?;
    let text = package_root.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        anyhow::bail!("package root must be an absolute local drive path");
    }
    Ok(())
}
