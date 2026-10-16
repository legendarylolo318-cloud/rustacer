//! Windows services via the Service Control Manager.

use crate::admin::is_elevated;
use crate::error::{CoreError, Result};
use crate::util::wide;
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::System::Services::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceState {
    Running,
    Stopped,
    Paused,
    Pending,
    #[default]
    Unknown,
}

impl ServiceState {
    pub fn label(self) -> &'static str {
        match self {
            ServiceState::Running => "Running",
            ServiceState::Stopped => "Stopped",
            ServiceState::Paused => "Paused",
            ServiceState::Pending => "Pending",
            ServiceState::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartType {
    Automatic,
    AutomaticDelayed,
    Manual,
    Disabled,
    Boot,
    System,
}

impl StartType {
    pub fn label(self) -> &'static str {
        match self {
            StartType::Automatic => "Automatic",
            StartType::AutomaticDelayed => "Automatic (Delayed)",
            StartType::Manual => "Manual",
            StartType::Disabled => "Disabled",
            StartType::Boot => "Boot",
            StartType::System => "System",
        }
    }
    /// Choices the user may apply.
    pub const SETTABLE: [StartType; 4] = [
        StartType::Automatic,
        StartType::AutomaticDelayed,
        StartType::Manual,
        StartType::Disabled,
    ];
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub display_name: String,
    pub state: ServiceState,
    pub start_type: StartType,
    pub description: String,
    pub pid: u32,
}

// ---------- protection lists (pure, unit-tested) ----------

/// Services that must never be disabled or stopped from Rustacer.
pub const PROTECTED: &[&str] = &[
    "rpcss",
    "dcomlaunch",
    "lsm",
    "windefend",
    "eventlog",
    "winmgmt",
    "plugplay",
    "power",
    "bfe",
    "mpssvc",
    "samss",
    "profsvc",
    "wscsvc",
    "cryptsvc",
    "trustedinstaller",
    "rpceptmapper",
    "lanmanworkstation",
    "sens",
    "schedule",
    "usermanager",
    "coremessagingregistrar",
    "gpsvc",
];

/// Services that merit a warning before any change.
pub const CRITICAL: &[&str] = &[
    "audiosrv",
    "dhcp",
    "dnscache",
    "nlasvc",
    "netprofm",
    "wlansvc",
    "themes",
    "spooler",
    "wuauserv",
    "bits",
    "lanmanserver",
    "msiserver",
    "w32time",
    "winhttpautoproxysvc",
    "sysmain",
    "vss",
    "appinfo",
    "token broker",
    "tokenbroker",
    "securityhealthservice",
    "sppsvc",
];

pub fn is_protected(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    PROTECTED.contains(&l.as_str())
}

pub fn is_critical(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    is_protected(&l) || CRITICAL.contains(&l.as_str())
}

/// Decide whether a requested action is allowed. `driver_updater_busy` makes
/// wuauserv untouchable while the driver updater is working.
pub fn check_change(name: &str, action: Action, driver_updater_busy: bool) -> Result<()> {
    let l = name.to_ascii_lowercase();
    if l == "wuauserv" && (driver_updater_busy || crate::drivers::wua::windows_update_in_use()) {
        return Err(CoreError::Blocked(
            "Windows Update is in use by the driver updater right now.".into(),
        ));
    }
    let destructive = matches!(
        action,
        Action::Stop | Action::Restart | Action::SetStartType(StartType::Disabled)
    );
    if is_protected(&l) && destructive {
        return Err(CoreError::Blocked(format!(
            "'{name}' is a protected system service and cannot be stopped or disabled."
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    SetStartType(StartType),
}

// ---------- SCM plumbing ----------

struct ScHandle(SC_HANDLE);
impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: handle was returned by the SCM and is closed exactly once.
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn open_scm(access: u32) -> Result<ScHandle> {
    // SAFETY: null machine/database selects the local active database.
    let h = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access)? };
    Ok(ScHandle(h))
}

fn open_service(scm: &ScHandle, name: &str, access: u32) -> Result<ScHandle> {
    let n = wide(name);
    // SAFETY: n is NUL-terminated and outlives the call.
    let h = unsafe { OpenServiceW(scm.0, PCWSTR::from_raw(n.as_ptr()), access)? };
    Ok(ScHandle(h))
}

fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: SCM returns NUL-terminated strings inside the enumeration buffer.
    unsafe { p.to_string().unwrap_or_default() }
}

fn state_from(cur: SERVICE_STATUS_CURRENT_STATE) -> ServiceState {
    match cur {
        SERVICE_RUNNING => ServiceState::Running,
        SERVICE_STOPPED => ServiceState::Stopped,
        SERVICE_PAUSED => ServiceState::Paused,
        SERVICE_START_PENDING
        | SERVICE_STOP_PENDING
        | SERVICE_CONTINUE_PENDING
        | SERVICE_PAUSE_PENDING => ServiceState::Pending,
        _ => ServiceState::Unknown,
    }
}

fn start_type_from(raw: u32, delayed: bool) -> StartType {
    match raw {
        0 => StartType::Boot,
        1 => StartType::System,
        2 if delayed => StartType::AutomaticDelayed,
        2 => StartType::Automatic,
        4 => StartType::Disabled,
        _ => StartType::Manual,
    }
}

/// Read a service's config (start type, description). Returns defaults on access failure.
fn read_details(scm: &ScHandle, name: &str) -> (StartType, String) {
    let Ok(svc) = open_service(scm, name, SERVICE_QUERY_CONFIG) else {
        return (StartType::Manual, String::new());
    };
    let mut start = StartType::Manual;
    // SAFETY: two-call buffer pattern; buffers sized by the first call.
    unsafe {
        let mut needed = 0u32;
        let _ = QueryServiceConfigW(svc.0, None, 0, &mut needed);
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            let cfg = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
            if QueryServiceConfigW(svc.0, Some(cfg), needed, &mut needed).is_ok() {
                let raw = (*cfg).dwStartType.0;
                let mut delayed = false;
                if raw == 2 {
                    let mut n2 = 0u32;
                    let mut d = SERVICE_DELAYED_AUTO_START_INFO::default();
                    let slice = std::slice::from_raw_parts_mut(
                        &mut d as *mut _ as *mut u8,
                        std::mem::size_of::<SERVICE_DELAYED_AUTO_START_INFO>(),
                    );
                    if QueryServiceConfig2W(
                        svc.0,
                        SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                        Some(slice),
                        &mut n2,
                    )
                    .is_ok()
                    {
                        delayed = d.fDelayedAutostart.as_bool();
                    }
                }
                start = start_type_from(raw, delayed);
            }
        }
        let mut desc = String::new();
        let mut needed = 0u32;
        let _ = QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, None, &mut needed);
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            if QueryServiceConfig2W(
                svc.0,
                SERVICE_CONFIG_DESCRIPTION,
                Some(&mut buf),
                &mut needed,
            )
            .is_ok()
            {
                let d = &*(buf.as_ptr() as *const SERVICE_DESCRIPTIONW);
                desc = pwstr_to_string(d.lpDescription);
            }
        }
        (start, desc)
    }
}

pub fn list_services() -> Result<Vec<ServiceInfo>> {
    let scm = open_scm(SC_MANAGER_ENUMERATE_SERVICE)?;
    let mut out = Vec::new();
    // SAFETY: standard two-call enumeration; entries are read from the buffer we own.
    unsafe {
        let mut needed = 0u32;
        let mut returned = 0u32;
        let mut resume = 0u32;
        let _ = EnumServicesStatusExW(
            scm.0,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            None,
            &mut needed,
            &mut returned,
            Some(&mut resume),
            PCWSTR::null(),
        );
        if needed == 0 {
            return Ok(out);
        }
        let mut buf = vec![0u8; needed as usize + 4096];
        resume = 0;
        EnumServicesStatusExW(
            scm.0,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            Some(&mut buf),
            &mut needed,
            &mut returned,
            Some(&mut resume),
            PCWSTR::null(),
        )?;
        let entries = std::slice::from_raw_parts(
            buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW,
            returned as usize,
        );
        for e in entries {
            let name = pwstr_to_string(e.lpServiceName);
            let (start_type, description) = read_details(&scm, &name);
            out.push(ServiceInfo {
                display_name: pwstr_to_string(e.lpDisplayName),
                state: state_from(e.ServiceStatusProcess.dwCurrentState),
                pid: e.ServiceStatusProcess.dwProcessId,
                name,
                start_type,
                description,
            });
        }
    }
    out.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });
    Ok(out)
}

pub fn query_state(name: &str) -> Result<ServiceState> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_QUERY_STATUS)?;
    let mut st = SERVICE_STATUS::default();
    // SAFETY: st is a valid out-struct.
    unsafe { QueryServiceStatus(svc.0, &mut st)? };
    Ok(state_from(st.dwCurrentState))
}

pub(crate) fn start_service(name: &str) -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_START | SERVICE_QUERY_STATUS)?;
    // SAFETY: no arguments passed.
    unsafe { StartServiceW(svc.0, None)? };
    info!("started service {name}");
    Ok(())
}

fn wait_for(name: &str, want: ServiceState, timeout_s: u64) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(timeout_s);
    while Instant::now() < end {
        if query_state(name)? == want {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(CoreError::Message(format!(
        "Timed out waiting for service '{name}' to become {}",
        want.label()
    )))
}

pub(crate) fn stop_and_wait(name: &str, timeout_s: u64) -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    let mut st = SERVICE_STATUS::default();
    // SAFETY: st is a valid out-struct.
    unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st)? };
    info!("stopping service {name}");
    wait_for(name, ServiceState::Stopped, timeout_s)
}

fn set_start_type(name: &str, t: StartType) -> Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_CHANGE_CONFIG)?;
    let raw = match t {
        StartType::Automatic | StartType::AutomaticDelayed => SERVICE_AUTO_START,
        StartType::Manual => SERVICE_DEMAND_START,
        StartType::Disabled => SERVICE_DISABLED,
        _ => {
            return Err(CoreError::Blocked(
                "That start type cannot be set here.".into(),
            ))
        }
    };
    // SAFETY: SERVICE_NO_CHANGE / null leave other fields untouched.
    unsafe {
        ChangeServiceConfigW(
            svc.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            raw,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )?;
        let mut d = SERVICE_DELAYED_AUTO_START_INFO {
            fDelayedAutostart: (t == StartType::AutomaticDelayed).into(),
        };
        if matches!(t, StartType::Automatic | StartType::AutomaticDelayed) {
            ChangeServiceConfig2W(
                svc.0,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some(&mut d as *mut _ as *mut _),
            )?;
        }
    }
    info!("service {name} start type -> {}", t.label());
    Ok(())
}

/// Perform an action after the safety checks. Logged.
pub fn apply(name: &str, action: Action, driver_updater_busy: bool) -> Result<()> {
    check_change(name, action, driver_updater_busy)?;
    if !is_elevated() {
        return Err(CoreError::NeedsAdmin);
    }
    match action {
        Action::Start => start_service(name),
        Action::Stop => stop_and_wait(name, 30),
        Action::Restart => {
            if query_state(name)? != ServiceState::Stopped {
                stop_and_wait(name, 30)?;
            }
            start_service(name)
        }
        Action::SetStartType(t) => set_start_type(name, t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_list() {
        assert!(is_protected("RpcSs"));
        assert!(is_protected("WinDefend"));
        assert!(is_protected("mpssvc"));
        assert!(!is_protected("Spooler"));
    }

    #[test]
    fn disabling_protected_is_blocked() {
        assert!(check_change("RpcSs", Action::SetStartType(StartType::Disabled), false).is_err());
        assert!(check_change("EventLog", Action::Stop, false).is_err());
        assert!(check_change("RpcSs", Action::Start, false).is_ok());
        assert!(check_change("Spooler", Action::Stop, false).is_ok());
    }

    #[test]
    fn wuauserv_locked_while_driver_updater_busy() {
        assert!(check_change("wuauserv", Action::Stop, true).is_err());
        assert!(check_change("wuauserv", Action::Stop, false).is_ok());
    }

    #[test]
    fn critical_includes_protected() {
        assert!(is_critical("Spooler"));
        assert!(is_critical("rpcss"));
        assert!(!is_critical("SomeRandomSvc"));
    }

    #[test]
    fn start_type_mapping() {
        assert_eq!(start_type_from(2, true), StartType::AutomaticDelayed);
        assert_eq!(start_type_from(2, false), StartType::Automatic);
        assert_eq!(start_type_from(3, false), StartType::Manual);
        assert_eq!(start_type_from(4, false), StartType::Disabled);
    }
}
