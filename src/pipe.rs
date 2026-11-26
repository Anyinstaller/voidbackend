use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_IO_PENDING,
    ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Threading::{CreateEventW, Sleep, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::cli::{self, CONNECT_TIMEOUT_MS};
use crate::frame::{self, Frame};
use crate::winutil;

const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_SLICE_MS: u32 = 250;

struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self> {
        let handle =
            unsafe { CreateEventW(None, true, false, None) }.context("CreateEventW failed")?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn reset(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::ResetEvent(self.0);
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct PipeClient {
    handle: HANDLE,
}

impl PipeClient {
    pub fn connect(name: &str) -> Result<Self> {
        Self::connect_with_timeout(name, Duration::from_millis(CONNECT_TIMEOUT_MS as u64))
    }

    pub fn connect_with_timeout(name: &str, timeout: Duration) -> Result<Self> {
        let path = cli::pipe_path(name)?;
        let wide = winutil::wide_str(&path);
        let deadline = Instant::now() + timeout;
        loop {
            let result = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(
                        FILE_ATTRIBUTE_NORMAL.0
                            | FILE_FLAG_OVERLAPPED.0
                            | SECURITY_SQOS_PRESENT.0
                            | SECURITY_IDENTIFICATION.0,
                    ),
                    None,
                )
            };
            match result {
                Ok(handle) => return Ok(Self { handle }),
                Err(error)
                    if winutil::is_win32(&error, ERROR_PIPE_BUSY)
                        || winutil::is_win32(&error, ERROR_FILE_NOT_FOUND)
                        || winutil::is_win32(&error, ERROR_PATH_NOT_FOUND) => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("failed to connect to {path}"));
                }
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for named pipe {path}");
            }
            unsafe { Sleep(25) };
        }
    }

    pub fn read_frame(&self) -> Result<Frame> {
        let mut header = [0u8; 4];
        let first = self.read_some(&mut header, None)?;
        if first == 0 {
            bail!("pipe closed before a frame was received");
        }
        if first < header.len() {
            self.read_exact(
                &mut header[first..],
                Some(Instant::now() + PARTIAL_FRAME_TIMEOUT),
            )?;
        }
        let length = frame::decode_length(header)?;
        let mut body = vec![0u8; length];
        self.read_exact(&mut body, Some(Instant::now() + PARTIAL_FRAME_TIMEOUT))?;
        frame::parse_message(&body)
    }

    pub fn write_table(&self, table: &toml::Table) -> Result<()> {
        let encoded = frame::encode_table(table)?;
        self.write_all(&encoded)
    }

    fn read_exact(&self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<()> {
        let mut offset = 0usize;
        while offset < buffer.len() {
            let count = self.read_some(&mut buffer[offset..], deadline)?;
            if count == 0 {
                bail!("pipe closed in the middle of a frame");
            }
            offset += count;
        }
        Ok(())
    }

    fn read_some(&self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let event = Event::new()?;
        event.reset();
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..Default::default()
        };
        let result = unsafe { ReadFile(self.handle, Some(buffer), None, Some(&mut overlapped)) };
        match result {
            Ok(()) => self.completed_bytes(&overlapped),
            Err(error) if winutil::is_win32(&error, ERROR_IO_PENDING) => {
                self.wait_for_completion(&event, &overlapped, deadline)
            }
            Err(error)
                if winutil::is_win32(&error, ERROR_BROKEN_PIPE)
                    || winutil::is_win32(&error, ERROR_HANDLE_EOF) =>
            {
                Ok(0)
            }
            Err(error) => Err(error).context("ReadFile failed"),
        }
    }

    fn write_all(&self, buffer: &[u8]) -> Result<()> {
        let deadline = Some(Instant::now() + WRITE_TIMEOUT);
        let mut offset = 0usize;
        while offset < buffer.len() {
            let event = Event::new()?;
            event.reset();
            let mut overlapped = OVERLAPPED {
                hEvent: event.raw(),
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
                Ok(()) => self.completed_bytes(&overlapped)?,
                Err(error) if winutil::is_win32(&error, ERROR_IO_PENDING) => {
                    self.wait_for_completion(&event, &overlapped, deadline)?
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

    fn completed_bytes(&self, overlapped: &OVERLAPPED) -> Result<usize> {
        let mut transferred = 0u32;
        match unsafe { GetOverlappedResult(self.handle, overlapped, &mut transferred, false) } {
            Ok(()) => Ok(transferred as usize),
            Err(error)
                if winutil::is_win32(&error, ERROR_BROKEN_PIPE)
                    || winutil::is_win32(&error, ERROR_HANDLE_EOF) =>
            {
                Ok(0)
            }
            Err(error) => Err(error).context("GetOverlappedResult failed"),
        }
    }

    fn wait_for_completion(
        &self,
        event: &Event,
        overlapped: &OVERLAPPED,
        deadline: Option<Instant>,
    ) -> Result<usize> {
        loop {
            let wait = unsafe { WaitForSingleObject(event.raw(), WAIT_SLICE_MS) };
            if wait == WAIT_OBJECT_0 {
                return self.completed_bytes(overlapped);
            }
            if wait == WAIT_TIMEOUT {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    self.cancel_and_reap(overlapped);
                    bail!("named pipe operation timed out");
                }
                continue;
            }
            self.cancel_and_reap(overlapped);
            if wait == WAIT_FAILED {
                return Err(windows::core::Error::from_thread())
                    .context("WaitForSingleObject failed");
            }
            bail!("unexpected WaitForSingleObject result: {wait:?}");
        }
    }

    fn cancel_and_reap(&self, overlapped: &OVERLAPPED) {
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
            let mut transferred = 0u32;
            let _ = GetOverlappedResult(self.handle, overlapped, &mut transferred, true);
        }
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        unsafe {
            let _ = CancelIoEx(self.handle, None);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_aip_pipe_names() {
        assert!(PipeClient::connect_with_timeout("not-a-pipe", Duration::ZERO).is_err());
    }

    #[test]
    fn connection_attempts_time_out() {
        let started = Instant::now();
        let error = match PipeClient::connect_with_timeout(
            "aip-ffffffffffffffffffffffffffffffff-backend",
            Duration::from_millis(60),
        ) {
            Ok(_) => panic!("unexpectedly connected to a pipe"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }
}
