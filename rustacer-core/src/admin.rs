use crate::error::{CoreError, Result};
use crate::util::shell_execute;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// True when the current process token is elevated.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: standard token query; handle closed before return.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// Passed to the elevated copy so it does not hand over to the copy that is closing.
pub const RESTARTED_ARG: &str = "--restarted";

/// Relaunch this executable with a UAC prompt. The caller should exit
/// the current process if this returns Ok.
pub fn restart_as_admin() -> Result<()> {
    let exe = std::env::current_exe()?;
    shell_execute("runas", &exe.to_string_lossy(), Some(RESTARTED_ARG))
        .map_err(|e| CoreError::Message(format!("Elevation was cancelled or failed: {e}")))
}

/// Return Err(NeedsAdmin) when not elevated.
pub fn require_admin() -> Result<()> {
    if is_elevated() {
        Ok(())
    } else {
        Err(CoreError::NeedsAdmin)
    }
}
