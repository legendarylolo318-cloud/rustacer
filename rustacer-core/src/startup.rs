//! Startup apps: Run/RunOnce keys, Startup folders and logon scheduled tasks.
//! Disabling uses the same `StartupApproved` mechanism as Task Manager so it is reversible.

use crate::admin::is_elevated;
use crate::error::{CoreError, Result};
use crate::util::ComGuard;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::info;
use winreg::enums::*;
use winreg::{RegKey, RegValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hive {
    User,
    Machine,
}

impl Hive {
    fn root(self) -> RegKey {
        RegKey::predef(match self {
            Hive::User => HKEY_CURRENT_USER,
            Hive::Machine => HKEY_LOCAL_MACHINE,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Hive::User => "HKCU",
            Hive::Machine => "HKLM",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Run { hive: Hive, wow32: bool },
    RunOnce { hive: Hive },
    Folder { common: bool },
    Task,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Run { hive, wow32 } => {
                format!("{} Run{}", hive.label(), if *wow32 { " (32-bit)" } else { "" })
            }
            Source::RunOnce { hive } => format!("{} RunOnce", hive.label()),
            Source::Folder { common: false } => "Startup folder (user)".into(),
            Source::Folder { common: true } => "Startup folder (all users)".into(),
            Source::Task => "Task Scheduler (logon)".into(),
        }
    }
    pub fn needs_admin(&self) -> bool {
        matches!(
            self,
            Source::Run { hive: Hive::Machine, .. }
                | Source::RunOnce { hive: Hive::Machine }
                | Source::Folder { common: true }
                | Source::Task
        )
    }
}

#[derive(Debug, Clone)]
pub struct StartupEntry {
    pub name: String,
    pub command: String,
    pub source: Source,
    pub enabled: bool,
    /// Full path for folder items or the scheduler path (e.g. `\Vendor\Task`).
    pub location: String,
}

const RUN: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
const RUN_WOW: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const RUNONCE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce";
const APPROVED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const RUNONCE_DISABLED: &str = r"SOFTWARE\Rustacer\DisabledRunOnce";

// ---------- StartupApproved binary format (pure, tested) ----------

/// First byte: even values (0x02, 0x06) = enabled; odd (0x03) = disabled.
pub fn parse_approved(bytes: &[u8]) -> bool {
    bytes.first().is_none_or(|b| b % 2 == 0)
}

/// Build a 12-byte StartupApproved value.
pub fn make_approved(enabled: bool, filetime: u64) -> Vec<u8> {
    let mut v = vec![0u8; 12];
    if enabled {
        v[0] = 0x02;
    } else {
        v[0] = 0x03;
        v[4..12].copy_from_slice(&filetime.to_le_bytes());
    }
    v
}

fn now_filetime() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs + 11_644_473_600) * 10_000_000
}

fn approved_subkey(source: &Source) -> Option<(Hive, &'static str)> {
    match source {
        Source::Run { hive, wow32: false } => Some((*hive, "Run")),
        Source::Run { hive, wow32: true } => Some((*hive, "Run32")),
        Source::Folder { common: false } => Some((Hive::User, "StartupFolder")),
        Source::Folder { common: true } => Some((Hive::Machine, "StartupFolder")),
        _ => None,
    }
}

fn read_approved(hive: Hive, sub: &str) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    if let Ok(k) = hive.root().open_subkey(format!(r"{APPROVED}\{sub}")) {
        for (n, v) in k.enum_values().flatten() {
            map.insert(n.to_lowercase(), v.bytes.into_owned());
        }
    }
    map
}

fn set_approved(source: &Source, name: &str, enabled: bool) -> Result<()> {
    let Some((hive, sub)) = approved_subkey(source) else {
        return Err(CoreError::Message("This entry type has no enable/disable flag.".into()));
    };
    let (k, _) = hive
        .root()
        .create_subkey(format!(r"{APPROVED}\{sub}"))
        .map_err(|e| CoreError::Windows(format!("Cannot open StartupApproved: {e}")))?;
    let val = RegValue {
        bytes: make_approved(enabled, now_filetime()).into(),
        vtype: REG_BINARY,
    };
    k.set_raw_value(name, &val)
        .map_err(|e| CoreError::Windows(format!("Cannot write StartupApproved: {e}")))
}

// ---------- parsing (mock-friendly) ----------

/// Combine raw (name, command) pairs with approval data into entries.
pub fn build_entries(
    raw: Vec<(String, String)>,
    source: Source,
    approved: &HashMap<String, Vec<u8>>,
) -> Vec<StartupEntry> {
    raw.into_iter()
        .map(|(name, command)| {
            let enabled = approved
                .get(&name.to_lowercase())
                .map(|b| parse_approved(b))
                .unwrap_or(true);
            StartupEntry {
                location: name.clone(),
                name,
                command,
                source: source.clone(),
                enabled,
            }
        })
        .collect()
}

fn read_run_values(hive: Hive, path: &str) -> Vec<(String, String)> {
    let Ok(k) = hive.root().open_subkey(path) else {
        return vec![];
    };
    k.enum_values()
        .flatten()
        .filter_map(|(n, _)| k.get_value::<String, _>(&n).ok().map(|v| (n, v)))
        .collect()
}

fn startup_folder(common: bool) -> Option<PathBuf> {
    let base = if common {
        std::env::var_os("ProgramData").map(PathBuf::from)?
    } else {
        std::env::var_os("APPDATA").map(PathBuf::from)?
    };
    Some(base.join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

fn folder_entries(common: bool) -> Vec<StartupEntry> {
    let Some(dir) = startup_folder(common) else {
        return vec![];
    };
    let source = Source::Folder { common };
    let approved = read_approved(if common { Hive::Machine } else { Hive::User }, "StartupFolder");
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            let enabled = approved.get(&name.to_lowercase()).map(|b| parse_approved(b)).unwrap_or(true);
            out.push(StartupEntry {
                command: e.path().to_string_lossy().into_owned(),
                location: e.path().to_string_lossy().into_owned(),
                name,
                source: source.clone(),
                enabled,
            });
        }
    }
    out
}

pub fn list_entries() -> Vec<StartupEntry> {
    let mut all = vec![];
    for hive in [Hive::User, Hive::Machine] {
        let src = Source::Run { hive, wow32: false };
        let ap = read_approved(hive, "Run");
        all.extend(build_entries(read_run_values(hive, RUN), src, &ap));
        if hive == Hive::Machine {
            let src = Source::Run { hive, wow32: true };
            let ap = read_approved(hive, "Run32");
            all.extend(build_entries(read_run_values(hive, RUN_WOW), src, &ap));
        }
        let src = Source::RunOnce { hive };
        all.extend(build_entries(read_run_values(hive, RUNONCE), src.clone(), &HashMap::new()));
        // RunOnce entries parked by Rustacer's own disable.
        let parked = read_run_values(Hive::User, &format!(r"{RUNONCE_DISABLED}\{}", hive.label()));
        for mut e in build_entries(parked, src, &HashMap::new()) {
            e.enabled = false;
            all.push(e);
        }
    }
    all.extend(folder_entries(false));
    all.extend(folder_entries(true));
    match tasks::list_logon_tasks() {
        Ok(t) => all.extend(t),
        Err(e) => tracing::warn!("task scheduler enumeration failed: {e}"),
    }
    all.sort_by_key(|e| e.name.to_lowercase());
    all
}

// ---------- mutation ----------

fn check_rights(e: &StartupEntry) -> Result<()> {
    if e.source.needs_admin() && !is_elevated() {
        Err(CoreError::NeedsAdmin)
    } else {
        Ok(())
    }
}

fn run_path(source: &Source) -> Option<(Hive, String)> {
    match source {
        Source::Run { hive, wow32: false } => Some((*hive, RUN.into())),
        Source::Run { hive, wow32: true } => Some((*hive, RUN_WOW.into())),
        Source::RunOnce { hive } => Some((*hive, RUNONCE.into())),
        _ => None,
    }
}

pub fn set_enabled(e: &StartupEntry, enabled: bool) -> Result<()> {
    check_rights(e)?;
    match &e.source {
        Source::Task => tasks::set_task_enabled(&e.location, enabled)?,
        Source::RunOnce { hive } => {
            let parked_path = format!(r"{RUNONCE_DISABLED}\{}", hive.label());
            let (from_hive, from_path, to_hive, to_path) = if enabled {
                (Hive::User, parked_path, *hive, RUNONCE.to_string())
            } else {
                (*hive, RUNONCE.to_string(), Hive::User, parked_path)
            };
            let from = from_hive
                .root()
                .open_subkey_with_flags(&from_path, KEY_READ | KEY_WRITE)
                .map_err(|e| CoreError::Windows(e.to_string()))?;
            let (to, _) = to_hive
                .root()
                .create_subkey(&to_path)
                .map_err(|e| CoreError::Windows(e.to_string()))?;
            let v: String = from.get_value(&e.name).map_err(|x| CoreError::Windows(x.to_string()))?;
            to.set_value(&e.name, &v).map_err(|x| CoreError::Windows(x.to_string()))?;
            from.delete_value(&e.name).map_err(|x| CoreError::Windows(x.to_string()))?;
        }
        other => set_approved(other, &e.name, enabled)?,
    }
    info!("startup entry '{}' {}", e.name, if enabled { "enabled" } else { "disabled" });
    Ok(())
}

pub fn delete_entry(e: &StartupEntry) -> Result<()> {
    check_rights(e)?;
    match &e.source {
        Source::Task => tasks::delete_task(&e.location)?,
        Source::Folder { .. } => {
            std::fs::remove_file(&e.location)?;
            let _ = clear_approved(e);
        }
        Source::RunOnce { hive } if !e.enabled => {
            let k = Hive::User
                .root()
                .open_subkey_with_flags(format!(r"{RUNONCE_DISABLED}\{}", hive.label()), KEY_WRITE)
                .map_err(|x| CoreError::Windows(x.to_string()))?;
            k.delete_value(&e.name).map_err(|x| CoreError::Windows(x.to_string()))?;
        }
        src => {
            let (hive, path) = run_path(src).ok_or_else(|| CoreError::Message("Unsupported entry".into()))?;
            let k = hive
                .root()
                .open_subkey_with_flags(path, KEY_WRITE)
                .map_err(|x| CoreError::Windows(x.to_string()))?;
            k.delete_value(&e.name).map_err(|x| CoreError::Windows(x.to_string()))?;
            let _ = clear_approved(e);
        }
    }
    info!("startup entry '{}' deleted ({})", e.name, e.source.label());
    Ok(())
}

fn clear_approved(e: &StartupEntry) -> Result<()> {
    if let Some((hive, sub)) = approved_subkey(&e.source) {
        let k = hive
            .root()
            .open_subkey_with_flags(format!(r"{APPROVED}\{sub}"), KEY_WRITE)
            .map_err(|x| CoreError::Windows(x.to_string()))?;
        let _ = k.delete_value(&e.name);
    }
    Ok(())
}

/// Add a per-user Run entry for an executable plus arguments.
pub fn add_user_entry(name: &str, exe: &str, args: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.contains('\\') {
        return Err(CoreError::Message("Enter a valid entry name.".into()));
    }
    let p = std::path::Path::new(exe);
    if !p.is_file() {
        return Err(CoreError::Message(format!("'{exe}' is not a file.")));
    }
    let cmd = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {}", args.trim())
    };
    let (k, _) = Hive::User
        .root()
        .create_subkey(RUN)
        .map_err(|e| CoreError::Windows(e.to_string()))?;
    k.set_value(name, &cmd).map_err(|e| CoreError::Windows(e.to_string()))?;
    info!("added startup entry '{name}' -> {cmd}");
    Ok(())
}

// ---------- "start with Windows" for Rustacer itself ----------

const SELF_VALUE: &str = "Rustacer";

pub fn is_self_registered() -> bool {
    Hive::User
        .root()
        .open_subkey(RUN)
        .and_then(|k| k.get_value::<String, _>(SELF_VALUE))
        .is_ok()
}

pub fn set_self_registered(on: bool) -> Result<()> {
    let (k, _) = Hive::User
        .root()
        .create_subkey(RUN)
        .map_err(|e| CoreError::Windows(e.to_string()))?;
    if on {
        let exe = std::env::current_exe()?;
        k.set_value(SELF_VALUE, &format!("\"{}\"", exe.display()))
            .map_err(|e| CoreError::Windows(e.to_string()))?;
    } else {
        let _ = k.delete_value(SELF_VALUE);
    }
    Ok(())
}

// ---------- Task Scheduler (COM) ----------

mod tasks {
    use super::*;
    use windows::core::{Interface, BSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::TaskScheduler::*;
    use windows::Win32::System::Variant::VARIANT;

    fn service() -> Result<ITaskService> {
        // SAFETY: COM initialised by the caller's ComGuard.
        unsafe {
            let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            svc.Connect(&VARIANT::default(), &VARIANT::default(), &VARIANT::default(), &VARIANT::default())?;
            Ok(svc)
        }
    }

    fn walk(folder: &ITaskFolder, out: &mut Vec<StartupEntry>) -> windows::core::Result<()> {
        // SAFETY: plain COM calls on live interface pointers.
        unsafe {
            let path = folder.Path()?.to_string();
            // Skip Microsoft's built-in task tree; it is noise for startup management.
            if path.to_lowercase().starts_with(r"\microsoft") {
                return Ok(());
            }
            let tasks = folder.GetTasks(1)?; // TASK_ENUM_HIDDEN
            for i in 1..=tasks.Count()? {
                let Ok(task) = tasks.get_Item(&VARIANT::from(i)) else { continue };
                let Ok(def) = task.Definition() else { continue };
                let Ok(trigs) = def.Triggers() else { continue };
                let mut logon = false;
                let mut tc = 0i32;
                let _ = trigs.Count(&mut tc);
                for t in 1..=tc {
                    if let Ok(tr) = trigs.get_Item(t) {
                        let mut ty = TASK_TRIGGER_EVENT;
                        if tr.Type(&mut ty).is_ok() && ty == TASK_TRIGGER_LOGON {
                            logon = true;
                        }
                    }
                }
                if !logon {
                    continue;
                }
                let mut command = String::new();
                if let Ok(actions) = def.Actions() {
                    if let Ok(a) = actions.get_Item(1) {
                        if let Ok(exec) = a.cast::<IExecAction>() {
                            let (mut pb, mut ab) = (BSTR::new(), BSTR::new());
                            let _ = exec.Path(&mut pb);
                            let _ = exec.Arguments(&mut ab);
                            let (p, args) = (pb.to_string(), ab.to_string());
                            command = format!("{p} {args}").trim().to_string();
                        }
                    }
                }
                let enabled = task.Enabled().map(|b| b.as_bool()).unwrap_or(true);
                out.push(StartupEntry {
                    name: task.Name().map(|b| b.to_string()).unwrap_or_default(),
                    command,
                    source: Source::Task,
                    enabled,
                    location: task.Path().map(|b| b.to_string()).unwrap_or_default(),
                });
            }
            let subs = folder.GetFolders(0)?;
            for i in 1..=subs.Count()? {
                if let Ok(f) = subs.get_Item(&VARIANT::from(i)) {
                    let _ = walk(&f, out);
                }
            }
        }
        Ok(())
    }

    pub fn list_logon_tasks() -> Result<Vec<StartupEntry>> {
        let _com = ComGuard::new()?;
        let svc = service()?;
        let mut out = vec![];
        // SAFETY: COM call on a live interface.
        let root = unsafe { svc.GetFolder(&BSTR::from(r"\"))? };
        walk(&root, &mut out)?;
        Ok(out)
    }

    fn get_task(svc: &ITaskService, path: &str) -> Result<IRegisteredTask> {
        // SAFETY: COM call on a live interface.
        let folder = unsafe { svc.GetFolder(&BSTR::from(r"\"))? };
        Ok(unsafe { folder.GetTask(&BSTR::from(path))? })
    }

    pub fn set_task_enabled(path: &str, enabled: bool) -> Result<()> {
        let _com = ComGuard::new()?;
        let svc = service()?;
        let t = get_task(&svc, path)?;
        // SAFETY: COM call on a live interface.
        unsafe { t.SetEnabled(enabled.into())? };
        Ok(())
    }

    pub fn delete_task(path: &str) -> Result<()> {
        let _com = ComGuard::new()?;
        let svc = service()?;
        // SAFETY: COM call on a live interface.
        let folder = unsafe { svc.GetFolder(&BSTR::from(r"\"))? };
        unsafe { folder.DeleteTask(&BSTR::from(path), 0)? };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_parsing() {
        assert!(parse_approved(&[0x02, 0, 0, 0]));
        assert!(parse_approved(&[0x06, 0, 0, 0]));
        assert!(!parse_approved(&[0x03, 0, 0, 0]));
        assert!(parse_approved(&[]));
    }

    #[test]
    fn approved_roundtrip() {
        let on = make_approved(true, 123);
        let off = make_approved(false, 123);
        assert_eq!(on.len(), 12);
        assert!(parse_approved(&on));
        assert!(!parse_approved(&off));
        assert_eq!(u64::from_le_bytes(off[4..12].try_into().unwrap()), 123);
    }

    #[test]
    fn entries_respect_approval_data() {
        let raw = vec![
            ("OneDrive".to_string(), "C:\\od.exe".to_string()),
            ("Spotify".to_string(), "C:\\sp.exe".to_string()),
            ("Other".to_string(), "C:\\o.exe".to_string()),
        ];
        let mut ap = HashMap::new();
        ap.insert("spotify".to_string(), make_approved(false, 1));
        ap.insert("onedrive".to_string(), make_approved(true, 0));
        let e = build_entries(raw, Source::Run { hive: Hive::User, wow32: false }, &ap);
        assert!(e[0].enabled);
        assert!(!e[1].enabled);
        assert!(e[2].enabled, "no approval record means enabled");
    }

    #[test]
    fn admin_requirements() {
        assert!(Source::Task.needs_admin());
        assert!(!Source::Run { hive: Hive::User, wow32: false }.needs_admin());
        assert!(Source::Run { hive: Hive::Machine, wow32: true }.needs_admin());
    }
}

