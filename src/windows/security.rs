use anyhow::{Result, ensure};
use std::{os::windows::ffi::OsStrExt, path::Path};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
    },
};

/// Private per-user engine state, including inherited permissions for new keys.
/// Administrators retain their OS-level ability to take ownership.
pub fn protect(path: &Path, directory: bool) -> Result<()> {
    ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "Private state cannot be a symbolic link"
    );
    let inherit = if directory { "OICI" } else { "" };
    let sddl: Vec<_> = format!(
        "D:P(A;{inherit};FA;;;{})(A;{inherit};FA;;;SY)",
        super::ipc::user_sid()?
    )
    .encode_utf16()
    .chain([0])
    .collect();
    let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let mut descriptor = std::ptr::null_mut();
        ensure!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut()
            ) != 0,
            "Cannot create private file permissions"
        );
        let ok = SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        );
        LocalFree(descriptor);
        ensure!(
            ok != 0,
            "Cannot protect private state: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}
