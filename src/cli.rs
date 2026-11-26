use anyhow::{bail, Context, Result};
use std::ffi::OsString;

pub const PIPE_ARGUMENT: &str = "--aip-pipe-name";
pub const CONNECT_TIMEOUT_MS: u32 = 30_000;

pub fn parse_pipe_name<I>(arguments: I) -> Result<String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let mut pipe_name = None;

    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        if text == PIPE_ARGUMENT {
            if pipe_name.is_some() {
                bail!("{PIPE_ARGUMENT} may only be specified once");
            }
            let value = arguments
                .next()
                .context("--aip-pipe-name requires a value")?;
            pipe_name = Some(value);
        } else if let Some(value) = text.strip_prefix("--aip-pipe-name=") {
            if pipe_name.is_some() {
                bail!("{PIPE_ARGUMENT} may only be specified once");
            }
            pipe_name = Some(OsString::from(value));
        }
        // AIP requires all unrelated arguments to be ignored.
    }

    let pipe_name = pipe_name.context("missing required --aip-pipe-name argument")?;
    let pipe_name = pipe_name
        .to_str()
        .context("--aip-pipe-name is not valid UTF-8")?;
    validate_pipe_name(pipe_name)?;
    Ok(pipe_name.to_string())
}

pub fn validate_pipe_name(name: &str) -> Result<()> {
    let Some(session_id) = name
        .strip_prefix("aip-")
        .and_then(|name| name.strip_suffix("-backend"))
    else {
        bail!("pipe name must match aip-<random-session-id>-backend");
    };
    if !(32..=128).contains(&session_id.len()) {
        bail!("pipe session id must contain 32 to 128 hexadecimal characters");
    }
    if !session_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("pipe session id must contain only hexadecimal characters");
    }
    Ok(())
}

pub fn pipe_path(name: &str) -> Result<String> {
    validate_pipe_name(name)?;
    Ok(format!(r"\\.\pipe\{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_required_argument_and_ignores_others() {
        let name = "aip-0123456789abcdef0123456789abcdef-backend";
        assert_eq!(
            parse_pipe_name(args(&["backend.exe", "--ignored", "x", "--outer"]))
                .unwrap_err()
                .to_string(),
            "missing required --aip-pipe-name argument"
        );
        assert_eq!(
            parse_pipe_name(args(&[
                "backend.exe",
                "--ignored",
                "x",
                "--aip-pipe-name",
                name,
                "--outer",
                "y",
            ]))
            .unwrap(),
            name
        );
        assert_eq!(
            parse_pipe_name(args(&["backend.exe", &format!("--aip-pipe-name={name}")])).unwrap(),
            name
        );
    }

    #[test]
    fn rejects_unsafe_pipe_names() {
        for value in [
            "",
            r"aip-0123456789abcdef0123456789abcdef-client",
            r"aip-short-backend",
            r"aip-0123456789abcdef0123456789abcdeg-backend",
            r"aip-0123456789abcdef0123456789abcdef-backend\evil",
        ] {
            assert!(validate_pipe_name(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_duplicate_argument() {
        let name = "aip-0123456789abcdef0123456789abcdef-backend";
        assert!(parse_pipe_name(args(&[
            "backend.exe",
            "--aip-pipe-name",
            name,
            "--aip-pipe-name",
            name
        ]))
        .is_err());
    }
}
