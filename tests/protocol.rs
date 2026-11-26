#![cfg(windows)]

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use voidbackend::frame;
use voidbackend::winutil;

struct PipeServer {
    handle: HANDLE,
    path: String,
}

impl PipeServer {
    fn create(name: &str) -> Result<Self> {
        let path = format!(r"\\.\pipe\{name}");
        let wide = winutil::wide_str(&path);
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_OVERLAPPED.0),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1024 * 1024,
                1024 * 1024,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            return Err(windows::core::Error::from_thread()).context("CreateNamedPipeW failed");
        }
        Ok(Self { handle, path })
    }

    fn connect(&self, timeout: Duration) -> Result<()> {
        let event = OwnedHandle::event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let result = unsafe { ConnectNamedPipe(self.handle, Some(&mut overlapped)) };
        match result {
            Ok(()) => Ok(()),
            Err(error) if winutil::is_win32(&error, ERROR_IO_PENDING) => {
                wait_io(self.handle, event.0, &overlapped, timeout)?;
                Ok(())
            }
            Err(error) if winutil::is_win32(&error, ERROR_PIPE_CONNECTED) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("failed to connect {}", self.path)),
        }
    }

    fn write_all(&self, buffer: &[u8], timeout: Duration) -> Result<()> {
        let mut offset = 0usize;
        while offset < buffer.len() {
            let event = OwnedHandle::event()?;
            event.reset();
            let mut overlapped = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let result = unsafe {
                WriteFile(
                    self.handle,
                    Some(&buffer[offset..]),
                    None,
                    Some(&mut overlapped),
                )
            };
            let count = match result {
                Ok(()) => completed_bytes(self.handle, &overlapped)?,
                Err(error) if winutil::is_win32(&error, ERROR_IO_PENDING) => {
                    wait_io(self.handle, event.0, &overlapped, timeout)?
                }
                Err(error) => return Err(error).context("WriteFile failed"),
            };
            if count == 0 {
                bail!("pipe write made no progress");
            }
            offset += count;
        }
        Ok(())
    }

    fn read_exact(&self, buffer: &mut [u8], timeout: Duration) -> Result<()> {
        let mut offset = 0usize;
        let deadline = Instant::now() + timeout;
        while offset < buffer.len() {
            let event = OwnedHandle::event()?;
            event.reset();
            let mut overlapped = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let result = unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut buffer[offset..]),
                    None,
                    Some(&mut overlapped),
                )
            };
            let count = match result {
                Ok(()) => completed_bytes(self.handle, &overlapped)?,
                Err(error) if winutil::is_win32(&error, ERROR_IO_PENDING) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    wait_io(self.handle, event.0, &overlapped, remaining)?
                }
                Err(error) if winutil::is_win32(&error, ERROR_BROKEN_PIPE) => 0,
                Err(error) => return Err(error).context("ReadFile failed"),
            };
            if count == 0 {
                bail!("pipe closed before the response was complete");
            }
            offset += count;
        }
        Ok(())
    }

    fn read_frame(&self, timeout: Duration) -> Result<frame::Frame> {
        let mut header = [0u8; 4];
        self.read_exact(&mut header, timeout)?;
        let length = frame::decode_length(header)?;
        let mut body = vec![0u8; length];
        self.read_exact(&mut body, timeout)?;
        frame::parse_message(&body)
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        unsafe {
            let _ = DisconnectNamedPipe(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn wait_io(
    handle: HANDLE,
    event: HANDLE,
    overlapped: &OVERLAPPED,
    timeout: Duration,
) -> Result<usize> {
    let wait = unsafe { WaitForSingleObject(event, timeout.as_millis() as u32) };
    if wait == WAIT_TIMEOUT {
        cancel_and_reap(handle, overlapped);
        bail!("pipe I/O timed out");
    }
    if wait != WAIT_OBJECT_0 {
        cancel_and_reap(handle, overlapped);
        return Err(windows::core::Error::from_thread()).context("WaitForSingleObject failed");
    }
    completed_bytes(handle, overlapped)
}

fn completed_bytes(handle: HANDLE, overlapped: &OVERLAPPED) -> Result<usize> {
    let mut transferred = 0u32;
    unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, false) }
        .context("GetOverlappedResult failed")?;
    Ok(transferred as usize)
}

fn cancel_and_reap(handle: HANDLE, overlapped: &OVERLAPPED) {
    unsafe {
        let _ = CancelIoEx(handle, Some(overlapped));
        let mut transferred = 0u32;
        let _ = GetOverlappedResult(handle, overlapped, &mut transferred, true);
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn event() -> Result<Self> {
        let handle =
            unsafe { CreateEventW(None, true, false, None) }.context("CreateEventW failed")?;
        Ok(Self(handle))
    }

    fn reset(&self) {
        unsafe {
            let _ = ResetEvent(self.0);
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_payload(path: &std::path::Path) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut writer = zip::ZipWriter::new(file);
    writer.start_file("Example.exe", zip::write::SimpleFileOptions::default())?;
    writer.write_all(b"executable")?;
    writer.finish()?;
    Ok(())
}

#[test]
fn complete_aip_installation_session() -> Result<()> {
    let package = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let target = destination.path().join("apps").join("Example");
    write_payload(&package.path().join("payload.zip"))?;
    let config = format!(
        "protocol = 1\nname = \"Example\"\n\n[defaults]\ninstallLocation = '{}'\ncreateStartMenuFolder = false\ncreateDesktopShortcut = false\n",
        target.display()
    );
    std::fs::write(package.path().join("config.toml"), config)?;

    let session_id = winutil::random_hex::<16>()?;
    let pipe_name = format!("aip-{session_id}-backend");
    let server = PipeServer::create(&pipe_name)?;
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_backend"))
            .current_dir(package.path())
            .arg("--aip-pipe-name")
            .arg(&pipe_name)
            .arg("--ignored")
            .arg("value")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn backend")?,
    );

    server.connect(Duration::from_secs(10))?;
    let mut frames = Vec::new();
    frames.extend(frame::encode_frame(
        format!(
            "type = \"setInstallLocation\"\nvalue = '{}'",
            target.display()
        )
        .as_bytes(),
    )?);
    frames.extend(frame::encode_frame(
        b"type = \"setCreateStartMenuFolder\"\nvalue = false",
    )?);
    frames.extend(frame::encode_frame(
        b"type = \"setCreateDesktopShortcut\"\nvalue = false",
    )?);
    frames.extend(frame::encode_frame(b"type = \"startInstall\"")?);
    server.write_all(&frames, Duration::from_secs(10))?;

    let mut last_percent = 0i64;
    let mut saw_progress = false;
    let finished = loop {
        let response = server.read_frame(Duration::from_secs(30))?;
        match response.message_type.as_str() {
            "installProgress" => {
                saw_progress = true;
                let percent = response
                    .document
                    .get("percent")
                    .and_then(toml::Value::as_integer)
                    .unwrap_or(-1);
                assert!((0..=100).contains(&percent));
                assert!(percent >= last_percent);
                last_percent = percent;
                assert!(response
                    .document
                    .get("stage")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|stage| !stage.is_empty()));
            }
            "installFinished" => break response,
            other => bail!("unexpected backend message: {other}"),
        }
    };
    assert!(saw_progress, "backend did not send installProgress");
    assert!(last_percent >= 99, "last progress was {last_percent}");
    assert_eq!(
        finished
            .document
            .get("success")
            .and_then(toml::Value::as_bool),
        Some(true),
        "response: {:?}",
        finished.document
    );
    assert_eq!(std::fs::read(target.join("Example.exe"))?, b"executable");
    assert_eq!(
        std::fs::read_dir(target.parent().unwrap())?
            .filter_map(Result::ok)
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".aip-stage-"))
            .count(),
        0
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait()? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.0.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            assert!(status.success(), "backend failed: {stderr}");
            break;
        }
        if Instant::now() >= deadline {
            bail!("backend did not exit after installFinished");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}
