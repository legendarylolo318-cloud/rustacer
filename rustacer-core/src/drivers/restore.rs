//! System Restore: create a point before driver installs, list existing points, delete one.

use crate::admin::{is_elevated, require_admin};
use crate::error::{CoreError, Result};
use crate::util::ComGuard;
use serde::Deserialize;
use tracing::info;
use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Restore::{
    RESTOREPOINTINFOW, RESTOREPOINTINFO_EVENT_TYPE, RESTOREPOINTINFO_TYPE, STATEMGRSTATUS,
};

// srclient.dll (System Restore) is missing on Windows Server and some stripped-down
// installs. Linking it statically would stop Rustacer from starting at all there, so it
// is loaded on demand and its absence becomes an ordinary error.
type SetFn = unsafe extern "system" fn(*const RESTOREPOINTINFOW, *mut STATEMGRSTATUS) -> BOOL;
type RemoveFn = unsafe extern "system" fn(u32) -> u32;

struct SrClient(HMODULE);

impl SrClient {
    fn load() -> Option<Self> {
        // SAFETY: loads a system DLL from System32 only; the handle is freed in Drop.
        unsafe { LoadLibraryExW(w!("srclient.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .ok()
            .map(SrClient)
    }

    fn set_restore_point(&self) -> Option<SetFn> {
        // SAFETY: the export has exactly this documented signature.
        unsafe {
            GetProcAddress(self.0, s!("SRSetRestorePointW"))
                .map(|f| std::mem::transmute::<_, SetFn>(f))
        }
    }

    fn remove_restore_point(&self) -> Option<RemoveFn> {
        // SAFETY: the export has exactly this documented signature.
        unsafe {
            GetProcAddress(self.0, s!("SRRemoveRestorePoint"))
                .map(|f| std::mem::transmute::<_, RemoveFn>(f))
        }
    }
}

impl Drop for SrClient {
    fn drop(&mut self) {
        // SAFETY: handle came from LoadLibraryExW and is freed once.
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

const NOT_AVAILABLE: &str = "System Restore is not available on this edition of Windows";

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
        1168 => "that restore point does not exist".into(),
        1460 => "a restore point could not be created right now (Windows allows one every 24 hours by default)".into(),
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
    let Some(lib) = SrClient::load() else {
        return RestoreOutcome::Failed(NOT_AVAILABLE.into());
    };
    let Some(set) = lib.set_restore_point() else {
        return RestoreOutcome::Failed(NOT_AVAILABLE.into());
    };
    let mut status = STATEMGRSTATUS::default();
    // SAFETY: both structs are valid for the duration of the call; `lib` keeps the DLL loaded.
    let ok = unsafe { set(&info, &mut status) };
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
    let Some(lib) = SrClient::load() else { return };
    let Some(set) = lib.set_restore_point() else {
        return;
    };
    let mut status = STATEMGRSTATUS::default();
    // SAFETY: both structs are valid for the duration of the call; `lib` keeps the DLL loaded.
    let _ = unsafe { set(&info, &mut status) };
}

// ---------- listing / deleting existing restore points ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePoint {
    pub sequence: u32,
    pub description: String,
    /// "YYYY-MM-DD HH:MM" (UTC as reported by WMI), or empty.
    pub created: String,
    pub kind: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename = "SystemRestore")]
#[serde(rename_all = "PascalCase")]
struct RawPoint {
    sequence_number: Option<u32>,
    description: Option<String>,
    creation_time: Option<String>,
    restore_point_type: Option<u32>,
}

/// Restore point type codes used by System Restore.
pub fn kind_label(t: u32) -> String {
    match t {
        0 => "Application install".into(),
        1 => "Application uninstall".into(),
        6 => "Restore operation".into(),
        7 => "System checkpoint".into(),
        10 => "Device driver install".into(),
        12 => "Settings change".into(),
        13 => "Cancelled operation".into(),
        16 => "Manual / backup recovery".into(),
        n => format!("Type {n}"),
    }
}

/// CIM datetime `20261016123045.000000-000` -> `2026-10-16 12:30`.
pub fn format_cim_datetime(s: &str) -> String {
    let d = s.get(0..12).unwrap_or("");
    if d.len() == 12 && d.chars().all(|c| c.is_ascii_digit()) {
        format!(
            "{}-{}-{} {}:{}",
            &d[0..4],
            &d[4..6],
            &d[6..8],
            &d[8..10],
            &d[10..12]
        )
    } else {
        String::new()
    }
}

/// List existing restore points, newest first. Needs administrator rights.
pub fn list_restore_points() -> Result<Vec<RestorePoint>> {
    require_admin()?;
    let _com = ComGuard::new()?;
    let con = wmi::WMIConnection::with_namespace_path(r"root\default")?;
    let raw: Vec<RawPoint> = con.query()?;
    let mut v: Vec<RestorePoint> = raw
        .into_iter()
        .filter_map(|r| {
            Some(RestorePoint {
                sequence: r.sequence_number?,
                description: r.description.unwrap_or_default(),
                created: r
                    .creation_time
                    .as_deref()
                    .map(format_cim_datetime)
                    .unwrap_or_default(),
                kind: kind_label(r.restore_point_type.unwrap_or(u32::MAX)),
            })
        })
        .collect();
    v.sort_by_key(|r| std::cmp::Reverse(r.sequence));
    Ok(v)
}

/// Permanently delete one restore point. Needs administrator rights; cannot be undone.
pub fn delete_restore_point(sequence: u32) -> Result<()> {
    require_admin()?;
    let lib = SrClient::load().ok_or_else(|| CoreError::Message(NOT_AVAILABLE.into()))?;
    let remove = lib
        .remove_restore_point()
        .ok_or_else(|| CoreError::Message(NOT_AVAILABLE.into()))?;
    // SAFETY: takes only a sequence number; `lib` keeps the DLL loaded for the call.
    let code = unsafe { remove(sequence) };
    if code == 0 {
        info!("deleted restore point #{sequence}");
        Ok(())
    } else {
        Err(CoreError::Message(format!(
            "Could not delete restore point #{sequence}: {}",
            describe_status(code)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_dates() {
        assert_eq!(kind_label(10), "Device driver install");
        assert_eq!(kind_label(99), "Type 99");
        assert_eq!(
            format_cim_datetime("20261016123045.000000-000"),
            "2026-10-16 12:30"
        );
        assert_eq!(format_cim_datetime("garbage"), "");
        assert_eq!(format_cim_datetime(""), "");
    }

    #[test]
    fn listing_and_deleting_require_admin() {
        if crate::admin::is_elevated() {
            return; // never touch real restore points from tests
        }
        assert!(matches!(list_restore_points(), Err(CoreError::NeedsAdmin)));
        assert!(matches!(
            delete_restore_point(1),
            Err(CoreError::NeedsAdmin)
        ));
    }

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
