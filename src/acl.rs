use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::winutil;

pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> Result<Self> {
        let wide = winutil::wide_str(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW failed")?;
        }
        Ok(Self(descriptor))
    }

    pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }
}

pub fn security_attributes(descriptor: &SecurityDescriptor) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr().0,
        bInheritHandle: BOOL(0),
    }
}

pub fn private_directory_sddl(sid: &str) -> String {
    format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid})")
}

pub fn current_user_sid_string() -> Result<String> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("OpenProcessToken failed")?;
    }
    let result = token_user_sid(token);
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

fn token_user_sid(token: HANDLE) -> Result<String> {
    let mut needed = 0u32;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    if needed == 0 {
        return Err(windows::core::Error::from_thread())
            .context("GetTokenInformation(TokenUser) failed");
    }
    let mut buffer = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .context("GetTokenInformation(TokenUser) failed")?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        if user.User.Sid.0.is_null() {
            bail!("current token has no user SID");
        }
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)
            .context("ConvertSidToStringSidW failed")?;
        let value = text.to_string().context("SID is not valid UTF-16");
        let _ = LocalFree(Some(HLOCAL(text.0 as *mut c_void)));
        value
    }
}

/// Create a directory with `CREATE_NEW` semantics. `false` means the path
/// already exists and must be validated by the caller.
pub fn create_directory_exclusive(path: &Path, descriptor: &SecurityDescriptor) -> Result<bool> {
    let wide = winutil::wide(winutil::verbatim(path));
    let attributes = security_attributes(descriptor);
    let result = unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&attributes)) };
    match result {
        Ok(()) => Ok(true),
        Err(error) if winutil::is_win32(&error, ERROR_ALREADY_EXISTS) => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("CreateDirectoryW failed for {}", path.display()))
        }
    }
}

pub fn create_private_directory(path: &Path) -> Result<bool> {
    let sid = current_user_sid_string()?;
    let descriptor = SecurityDescriptor::from_sddl(&private_directory_sddl(&sid))?;
    create_directory_exclusive(path, &descriptor)
}

/// Re-enable DACL inheritance for an installed object. This is used after the
/// private staging directory has been committed to its final location.
pub fn enable_dacl_inheritance(path: &Path) -> Result<()> {
    let mut wide = winutil::wide(winutil::verbatim(path));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let get_result = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if get_result != ERROR_SUCCESS {
        bail!(
            "GetNamedSecurityInfoW failed for {}: error {}",
            path.display(),
            get_result.0
        );
    }

    let set_result = unsafe {
        SetNamedSecurityInfoW(
            PWSTR(wide.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    if set_result != ERROR_SUCCESS {
        bail!(
            "SetNamedSecurityInfoW failed for {}: error {}",
            path.display(),
            set_result.0
        );
    }
    Ok(())
}

pub fn verify_directory(path: &Path) -> Result<()> {
    let attributes = winutil::attributes_if_exists(path)?
        .with_context(|| format!("directory does not exist: {}", path.display()))?;
    if attributes.0 & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        bail!("path is not a directory: {}", path.display());
    }
    if winutil::is_reparse_point(FILE_FLAGS_AND_ATTRIBUTES(attributes.0)) {
        bail!("path is a reparse point: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_is_available() {
        let sid = current_user_sid_string().unwrap();
        assert!(sid.starts_with("S-1-"));
    }

    #[test]
    fn creates_a_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        assert!(create_private_directory(&path).unwrap());
        verify_directory(&path).unwrap();
        enable_dacl_inheritance(&path).unwrap();
        std::fs::remove_dir(&path).unwrap();
    }
}
