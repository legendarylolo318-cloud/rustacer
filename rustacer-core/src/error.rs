use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("{0}")]
    Message(String),
    #[error("Administrator rights are required for this action.")]
    NeedsAdmin,
    #[error("Blocked for safety: {0}")]
    Blocked(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Windows error: {0}")]
    Windows(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Cancelled by user.")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl From<windows::core::Error> for CoreError {
    fn from(e: windows::core::Error) -> Self {
        CoreError::Windows(hresult_message(e.code().0, &e.message()))
    }
}

impl From<wmi::WMIError> for CoreError {
    fn from(e: wmi::WMIError) -> Self {
        CoreError::Windows(format!("WMI query failed: {e}"))
    }
}

/// Turn an HRESULT into a human-readable sentence.
pub fn hresult_message(hr: i32, os_message: &str) -> String {
    let known = match hr as u32 {
        0x8000_4005 => Some("Unspecified error"),
        0x8007_0005 => Some("Access denied - try restarting Rustacer as administrator"),
        0x8007_0002 => Some("File or item not found"),
        0x8007_0057 => Some("Invalid parameter"),
        0x8007_05B4 => Some("The operation timed out"),
        0x8024_0004 => Some("Windows Update: no network or service unavailable"),
        0x8024_001E => Some("Windows Update: service is shutting down"),
        0x8024_402C => Some("Windows Update: could not reach the update server (check proxy/network)"),
        0x8024_402F => Some("Windows Update: connection to the update server was interrupted"),
        0x8024_0024 => Some("Windows Update: no updates found"),
        0x8024_0022 => Some("Windows Update: all updates failed to download or install"),
        0x8024_AFFF => Some("Windows Update: unexpected agent error"),
        0x8024_0009 => Some("Windows Update: the operation was cancelled"),
        0x8024_000B => Some("Windows Update: the operation was cancelled"),
        0x8024_0016 => Some("Windows Update: another installation is in progress or a reboot is pending"),
        _ => None,
    };
    match known {
        Some(k) => format!("{k} (0x{:08X})", hr as u32),
        None => {
            let m = os_message.trim();
            if m.is_empty() {
                format!("Unknown error (0x{:08X})", hr as u32)
            } else {
                format!("{m} (0x{:08X})", hr as u32)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hresult_is_translated() {
        assert!(hresult_message(0x8007_0005u32 as i32, "").contains("Access denied"));
    }

    #[test]
    fn unknown_hresult_falls_back() {
        assert!(hresult_message(0x1234_5678, "").contains("Unknown error"));
        assert!(hresult_message(0x1234_5678, "boom").starts_with("boom"));
    }
}
