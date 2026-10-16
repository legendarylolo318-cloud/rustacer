//! System Restore point creation via SRSetRestorePointW.

use crate::admin::is_elevated;
use windows::Win32::System::Restore::{
    SRSetRestorePointW, RESTOREPOINTINFOW, RESTOREPOINTINFO_EVENT_TYPE, RESTOREPOINTINFO_TYPE,
    STATEMGRSTATUS,
};

const BEGIN_SYSTEM_CHANGE: u32 = 100;
const END_SYSTEM_CHANGE: u32 = 101;
const DEVICE_DRIVER_INSTALL: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    Created {
        sequence: i64,
    },
    /// Could not create one; the message explains why (System Protection off, throttled, ...).
    Failed(String),
}

fn describe_status(code: u32) -> String {
    match code {
        0 => "success".into(),
        1058 => "System Protection is turned off (System Restore service disabled)".into(),
        1460 | 1168 => "a restore point could not be created right now (Windows allows one every 24 hours by default)".into(),
        5 => "access denied - administrator rights are required".into(),
        n => format!("error {n}"),
    }
}

fn describe(text: &str) -> [u16; 256] {
    let mut buf = [0u16; 256];
    for (i, u) in text.encode_utf16().take(255).enumerate() {
        buf[i] = u;
    }
    buf
}

pub fn create_restore_point(description: &str) -> RestoreOutcome {
    if !is_elevated() {
        return RestoreOutcome::Failed("administrator rights are required".into());
    }
    let info = RESTOREPOINTINFOW {
        dwEventType: RESTOREPOINTINFO_EVENT_TYPE(BEGIN_SYSTEM_CHANGE),
        dwRestorePtType: RESTOREPOINTINFO_TYPE(DEVICE_DRIVER_INSTALL),
        llSequenceNumber: 0,
        szDescription: describe(description),
    };
    let mut status = STATEMGRSTATUS::default();
    // SAFETY: both structs are valid for the duration of the call.
    let ok = unsafe { SRSetRestorePointW(&info, &mut status) };
    let (code, seq) = ({ status.nStatus }.0, { status.llSequenceNumber });
    if ok.as_bool() && code == 0 {
        RestoreOutcome::Created { sequence: seq }
    } else {
        RestoreOutcome::Failed(describe_status(code))
    }
}

/// Close the restore point opened by `create_restore_point`.
pub fn finish_restore_point(sequence: i64) {
    let info = RESTOREPOINTINFOW {
        dwEventType: RESTOREPOINTINFO_EVENT_TYPE(END_SYSTEM_CHANGE),
        dwRestorePtType: RESTOREPOINTINFO_TYPE(DEVICE_DRIVER_INSTALL),
        llSequenceNumber: sequence,
        szDescription: [0; 256],
    };
    let mut status = STATEMGRSTATUS::default();
    // SAFETY: both structs are valid for the duration of the call.
    let _ = unsafe { SRSetRestorePointW(&info, &mut status) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text() {
        assert!(describe_status(1058).contains("turned off"));
        assert!(describe_status(99999).contains("99999"));
    }

    #[test]
    fn description_is_truncated_and_terminated() {
        let long = "x".repeat(1000);
        let d = describe(&long);
        assert_eq!(d[254], b'x' as u16);
        assert_eq!(d[255], 0);
    }
}
