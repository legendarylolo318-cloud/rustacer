//! Windows Update Agent (COM) driver search / download / install.
//! Windows Update is the only update source. Nothing is installed unless
//! the caller passes explicit update ids.

use super::installed::InstalledDriver;
use super::version::{Date, DriverVersion};
use crate::admin::require_admin;
use crate::error::{hresult_message, CoreError, Result};
use crate::util::ComGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use windows::core::{implement, Interface, Ref, BSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::UpdateAgent::*;
use windows::Win32::System::Variant::VARIANT;

/// Number of Windows Update operations (search/install) running in this process.
static WU_IN_USE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// True while Rustacer is searching or installing through Windows Update. The cleaner and
/// service control refuse to stop `wuauserv` or clear its cache while this is set.
pub fn windows_update_in_use() -> bool {
    WU_IN_USE.load(Ordering::SeqCst) > 0
}

/// RAII marker for an in-flight Windows Update operation.
pub struct WuInUse(());

impl WuInUse {
    pub fn new() -> Self {
        WU_IN_USE.fetch_add(1, Ordering::SeqCst);
        WuInUse(())
    }
}

impl Default for WuInUse {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WuInUse {
    fn drop(&mut self) {
        WU_IN_USE.fetch_sub(1, Ordering::SeqCst);
    }
}

pub const SEARCH_CRITERIA: &str = "IsInstalled=0 and IsHidden=0 and Type='Driver'";

#[derive(Debug, Clone, Default)]
pub struct DriverUpdate {
    pub update_id: String,
    pub revision: i32,
    pub title: String,
    pub device_name: String,
    pub manufacturer: String,
    pub driver_class: String,
    pub hardware_id: String,
    pub new_version: String,
    pub new_date: Option<Date>,
    pub size_bytes: u64,
    pub eula_accepted: bool,
    /// Filled in by `match_installed`.
    pub current_version: String,
    pub current_date: Option<Date>,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub stage: String,
    /// 0..=100 when determinate.
    pub percent: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub title: String,
    pub result: String,
    pub success: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InstallSummary {
    pub outcomes: Vec<UpdateOutcome>,
    pub reboot_required: bool,
}

// ---------- result codes (pure, tested) ----------

/// WUA `OperationResultCode`.
pub fn result_code_text(code: i32) -> &'static str {
    match code {
        0 => "Not started",
        1 => "In progress",
        2 => "Succeeded",
        3 => "Succeeded with errors",
        4 => "Failed",
        5 => "Aborted",
        _ => "Unknown result",
    }
}

pub fn result_code_ok(code: i32) -> bool {
    code == 2 || code == 3
}

/// Combine a result code and HRESULT into one readable line.
pub fn describe_result(code: i32, hresult: i32) -> String {
    if hresult == 0 {
        result_code_text(code).to_string()
    } else {
        format!(
            "{}: {}",
            result_code_text(code),
            hresult_message(hresult, "")
        )
    }
}

// ---------- matching ----------

fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Match an update to an installed device: first by any of the device's hardware or
/// compatible IDs, then by device name (WUA's DriverModel is usually the friendly name).
pub fn match_installed<'a>(
    u: &DriverUpdate,
    installed: &'a [InstalledDriver],
) -> Option<&'a InstalledDriver> {
    let want = norm(&u.hardware_id);
    if !want.is_empty() {
        if let Some(d) = installed
            .iter()
            .find(|d| d.hardware_ids.iter().any(|id| norm(id) == want))
        {
            return Some(d);
        }
    }
    let model = norm(&u.device_name);
    if model.is_empty() {
        return None;
    }
    let mut by_name = installed.iter().filter(|d| norm(&d.device_name) == model);
    let first = by_name.next()?;
    // Ambiguous names (several identical devices with different drivers) are not guessed.
    match by_name.next() {
        Some(second) if second.version != first.version => None,
        _ => Some(first),
    }
}

/// GPU (display) drivers are never offered or installed through Rustacer: they are large,
/// tightly coupled to vendor tooling, and a bad one can leave the machine without a usable
/// display. Users get the vendor's official page instead (Driver Updater > GPU).
pub fn is_gpu_update(u: &DriverUpdate, installed: &[InstalledDriver]) -> bool {
    let class = u.driver_class.trim().to_ascii_lowercase();
    let title = u.title.to_ascii_lowercase();
    class == "display"
        || title.contains("display driver")
        || title.contains("- display -")
        || match_installed(u, installed)
            .is_some_and(|d| d.device_class.eq_ignore_ascii_case("DISPLAY"))
}

/// Remove GPU updates; returns the titles that were dropped.
pub fn drop_gpu_updates(
    updates: &mut Vec<DriverUpdate>,
    installed: &[InstalledDriver],
) -> Vec<String> {
    let mut dropped = Vec::new();
    updates.retain(|u| {
        if is_gpu_update(u, installed) {
            dropped.push(u.title.clone());
            false
        } else {
            true
        }
    });
    dropped
}

pub fn fill_current(updates: &mut [DriverUpdate], installed: &[InstalledDriver]) {
    for u in updates {
        if let Some(d) = match_installed(u, installed) {
            u.current_version = d.version.clone();
            u.current_date = d.date;
            if u.device_name.is_empty() {
                u.device_name = d.device_name.clone();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    Newer,
    Same,
    /// Windows Update sometimes offers a lower version than what is installed.
    Older,
    /// No installed device matched, or a version could not be parsed.
    Unknown,
}

impl UpdateStatus {
    pub fn label(self) -> &'static str {
        match self {
            UpdateStatus::Newer => "Newer",
            UpdateStatus::Same => "Same version",
            UpdateStatus::Older => "Older (downgrade)",
            UpdateStatus::Unknown => "Unknown",
        }
    }
}

pub fn status(u: &DriverUpdate) -> UpdateStatus {
    match super::version::compare_versions(&u.new_version, &u.current_version) {
        Some(std::cmp::Ordering::Greater) => UpdateStatus::Newer,
        Some(std::cmp::Ordering::Equal) => UpdateStatus::Same,
        Some(std::cmp::Ordering::Less) => UpdateStatus::Older,
        None => UpdateStatus::Unknown,
    }
}

/// True if the update looks newer than what is installed (informational).
pub fn is_newer(u: &DriverUpdate) -> Option<bool> {
    let new = DriverVersion::parse(&u.new_version)?;
    let cur = DriverVersion::parse(&u.current_version)?;
    Some(new > cur)
}

// ---------- COM helpers ----------

fn session() -> Result<IUpdateSession> {
    // SAFETY: COM is initialised by the caller's ComGuard.
    Ok(unsafe { CoCreateInstance(&UpdateSession, None, CLSCTX_INPROC_SERVER)? })
}

// WUA rejects null completion callbacks; we poll, so these do nothing.
#[implement(ISearchCompletedCallback)]
struct SearchCb;
impl ISearchCompletedCallback_Impl for SearchCb_Impl {
    fn Invoke(
        &self,
        _: Ref<ISearchJob>,
        _: Ref<ISearchCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(IDownloadCompletedCallback)]
struct DownloadCb;
impl IDownloadCompletedCallback_Impl for DownloadCb_Impl {
    fn Invoke(
        &self,
        _: Ref<IDownloadJob>,
        _: Ref<IDownloadCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(IDownloadProgressChangedCallback)]
struct DownloadProgressCb;
impl IDownloadProgressChangedCallback_Impl for DownloadProgressCb_Impl {
    fn Invoke(
        &self,
        _: Ref<IDownloadJob>,
        _: Ref<IDownloadProgressChangedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(IInstallationProgressChangedCallback)]
struct InstallProgressCb;
impl IInstallationProgressChangedCallback_Impl for InstallProgressCb_Impl {
    fn Invoke(
        &self,
        _: Ref<IInstallationJob>,
        _: Ref<IInstallationProgressChangedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(IInstallationCompletedCallback)]
struct InstallCb;
impl IInstallationCompletedCallback_Impl for InstallCb_Impl {
    fn Invoke(
        &self,
        _: Ref<IInstallationJob>,
        _: Ref<IInstallationCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

fn step(name: &str, e: windows::core::Error) -> CoreError {
    CoreError::Windows(format!(
        "{name}: {}",
        hresult_message(e.code().0, &e.message())
    ))
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(CoreError::Cancelled)
    } else {
        Ok(())
    }
}

fn parse_update(u: &IUpdate) -> Result<DriverUpdate> {
    // SAFETY: COM property reads on a live interface.
    unsafe {
        let title = u.Title()?.to_string();
        let ident = u.Identity()?;
        let mut out = DriverUpdate {
            update_id: ident.UpdateID()?.to_string(),
            revision: ident.RevisionNumber()?,
            new_version: DriverVersion::find_in_text(&title)
                .map(|v| v.to_string())
                .unwrap_or_default(),
            title,
            eula_accepted: u.EulaAccepted().map(|b| b.as_bool()).unwrap_or(false),
            size_bytes: u.MaxDownloadSize().map(decimal_to_u64).unwrap_or(0),
            ..Default::default()
        };
        if let Ok(d) = u.cast::<IWindowsDriverUpdate>() {
            out.device_name = d.DriverModel().map(|b| b.to_string()).unwrap_or_default();
            out.manufacturer = d
                .DriverManufacturer()
                .map(|b| b.to_string())
                .unwrap_or_default();
            out.driver_class = d.DriverClass().map(|b| b.to_string()).unwrap_or_default();
            out.hardware_id = d
                .DriverHardwareID()
                .map(|b| b.to_string())
                .unwrap_or_default();
            out.new_date = d.DriverVerDate().ok().and_then(Date::from_ole);
        }
        Ok(out)
    }
}

fn decimal_to_u64(d: windows::Win32::Foundation::DECIMAL) -> u64 {
    // SAFETY: DECIMAL is plain data; Lo64 is the low 64 bits of the 96-bit mantissa.
    unsafe { d.Anonymous2.Lo64 }
}

fn search(
    session: &IUpdateSession,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<IUpdate>> {
    // SAFETY: COM calls on live interfaces; the job is polled until completion.
    unsafe {
        let searcher = session
            .CreateUpdateSearcher()
            .map_err(|e| step("CreateUpdateSearcher", e))?;
        let _ = searcher.SetOnline(true.into());
        let job = searcher
            .BeginSearch(
                &BSTR::from(SEARCH_CRITERIA),
                &ISearchCompletedCallback::from(SearchCb),
                &VARIANT::default(),
            )
            .map_err(|e| step("BeginSearch", e))?;
        let start = Instant::now();
        loop {
            if job.IsCompleted()?.as_bool() {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                let _ = job.RequestAbort();
                let _ = searcher.EndSearch(&job);
                return Err(CoreError::Cancelled);
            }
            progress(Progress {
                stage: format!(
                    "Searching Windows Update... {}s elapsed (this can take several minutes)",
                    start.elapsed().as_secs()
                ),
                percent: None,
            });
            std::thread::sleep(Duration::from_millis(500));
        }
        let result = searcher.EndSearch(&job).map_err(|e| step("EndSearch", e))?;
        let coll = result.Updates().map_err(|e| step("Updates", e))?;
        let mut v = Vec::new();
        for i in 0..coll.Count()? {
            v.push(coll.get_Item(i)?);
        }
        Ok(v)
    }
}

/// Search Windows Update for uninstalled driver updates. Runs on the calling
/// thread (call it from a worker); COM is initialised and released here.
pub fn search_driver_updates(
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<DriverUpdate>> {
    let _busy = WuInUse::new();
    let _com = ComGuard::new()?;
    let sess = session()?;
    progress(Progress {
        stage: "Contacting Windows Update...".into(),
        percent: None,
    });
    let found = search(&sess, cancel, progress)?;
    let mut out = Vec::with_capacity(found.len());
    for u in &found {
        match parse_update(u) {
            Ok(d) => out.push(d),
            Err(e) => warn!("skipping unreadable update: {e}"),
        }
    }
    info!("driver search found {} updates", out.len());
    Ok(out)
}

/// Download and install exactly the selected updates (by id). Requires admin.
/// The caller must have obtained the user's confirmation (and tried a restore point).
pub fn install_updates(
    selected_ids: &[String],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<InstallSummary> {
    require_admin()?;
    let _busy = WuInUse::new();
    if selected_ids.is_empty() {
        return Err(CoreError::Message("No updates were selected.".into()));
    }
    let _com = ComGuard::new()?;
    let sess = session()?;
    let found = search(&sess, cancel, progress)?;

    // SAFETY: COM calls on live interfaces; jobs are polled to completion.
    unsafe {
        let to_install: IUpdateCollection =
            CoCreateInstance(&UpdateCollection, None, CLSCTX_INPROC_SERVER)?;
        let mut titles = Vec::new();
        for u in &found {
            let id = u.Identity()?.UpdateID()?.to_string();
            if !selected_ids.iter().any(|s| s.eq_ignore_ascii_case(&id)) {
                continue;
            }
            let title = u.Title()?.to_string();
            // Defence in depth: refuse GPU drivers even if their id was passed in.
            if parse_update(u).is_ok_and(|d| is_gpu_update(&d, &[])) {
                return Err(CoreError::Blocked(format!(
                    "'{title}' is a GPU driver. Rustacer does not install GPU drivers; use the vendor's official page."
                )));
            }
            if !u.EulaAccepted()?.as_bool() {
                return Err(CoreError::Message(format!(
                    "'{title}' requires accepting a license agreement. Install it from Windows Settings > Windows Update."
                )));
            }
            to_install.Add(u)?;
            titles.push(title);
        }
        if titles.is_empty() {
            return Err(CoreError::Message(
                "The selected updates are no longer offered by Windows Update. Scan again.".into(),
            ));
        }
        info!("installing {} driver update(s)", titles.len());

        // ---- download ----
        check_cancel(cancel)?;
        let downloader = sess.CreateUpdateDownloader()?;
        downloader.SetUpdates(&to_install)?;
        let djob = downloader.BeginDownload(
            &IDownloadProgressChangedCallback::from(DownloadProgressCb),
            &IDownloadCompletedCallback::from(DownloadCb),
            &VARIANT::default(),
        )?;
        while !djob.IsCompleted()?.as_bool() {
            if cancel.load(Ordering::Relaxed) {
                let _ = djob.RequestAbort();
                let _ = downloader.EndDownload(&djob);
                return Err(CoreError::Cancelled);
            }
            if let Ok(p) = djob.GetProgress() {
                let idx = p.CurrentUpdateIndex().unwrap_or(0);
                progress(Progress {
                    stage: format!(
                        "Downloading {} ({}/{})",
                        titles
                            .get(idx as usize)
                            .map(String::as_str)
                            .unwrap_or("update"),
                        idx + 1,
                        titles.len()
                    ),
                    percent: p.PercentComplete().ok().map(|x| x.max(0) as u32),
                });
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        let dres = downloader.EndDownload(&djob)?;
        let mut summary = InstallSummary::default();
        let ready: IUpdateCollection =
            CoCreateInstance(&UpdateCollection, None, CLSCTX_INPROC_SERVER)?;
        for (i, title) in titles.iter().enumerate() {
            let r = dres.GetUpdateResult(i as i32)?;
            let code = r.ResultCode()?.0;
            if result_code_ok(code) {
                ready.Add(&to_install.get_Item(i as i32)?)?;
            } else {
                summary.outcomes.push(UpdateOutcome {
                    title: title.clone(),
                    result: format!(
                        "Download - {}",
                        describe_result(code, r.HResult().unwrap_or(0))
                    ),
                    success: false,
                });
            }
        }
        if ready.Count()? == 0 {
            return Ok(summary);
        }

        // ---- install (not cancellable once started) ----
        let installer = sess.CreateUpdateInstaller()?;
        installer.SetUpdates(&ready)?;
        let _ = installer.SetAllowSourcePrompts(false.into());
        let ijob = installer.BeginInstall(
            &IInstallationProgressChangedCallback::from(InstallProgressCb),
            &IInstallationCompletedCallback::from(InstallCb),
            &VARIANT::default(),
        )?;
        while !ijob.IsCompleted()?.as_bool() {
            if let Ok(p) = ijob.GetProgress() {
                let idx = p.CurrentUpdateIndex().unwrap_or(0);
                progress(Progress {
                    stage: format!(
                        "Installing update {} of {}",
                        idx + 1,
                        ready.Count().unwrap_or(0)
                    ),
                    percent: p.PercentComplete().ok().map(|x| x.max(0) as u32),
                });
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        let ires = installer.EndInstall(&ijob)?;
        summary.reboot_required = ires.RebootRequired()?.as_bool();
        for i in 0..ready.Count()? {
            let title = ready.get_Item(i)?.Title()?.to_string();
            let r = ires.GetUpdateResult(i)?;
            let code = r.ResultCode()?.0;
            summary.outcomes.push(UpdateOutcome {
                title,
                result: describe_result(code, r.HResult().unwrap_or(0)),
                success: result_code_ok(code),
            });
        }
        info!(
            "install finished: {} outcomes, reboot={}",
            summary.outcomes.len(),
            summary.reboot_required
        );
        Ok(summary)
    }
}

/// Ask Windows to restart in 10 seconds (the UI must confirm first).
pub fn reboot_now() -> Result<()> {
    let (code, out) = crate::util::run_hidden(
        "shutdown",
        &[
            "/r",
            "/t",
            "10",
            "/c",
            "Rustacer: restarting to finish driver installation",
        ],
    )?;
    if code == 0 {
        Ok(())
    } else {
        Err(CoreError::Message(format!(
            "Could not schedule restart: {}",
            out.trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_codes() {
        assert_eq!(result_code_text(2), "Succeeded");
        assert_eq!(result_code_text(4), "Failed");
        assert_eq!(result_code_text(5), "Aborted");
        assert_eq!(result_code_text(77), "Unknown result");
        assert!(result_code_ok(3));
        assert!(!result_code_ok(4));
    }

    #[test]
    fn describe_includes_hresult_text() {
        let s = describe_result(4, 0x8024_402Cu32 as i32);
        assert!(s.starts_with("Failed"));
        assert!(s.contains("update server"));
        assert_eq!(describe_result(2, 0), "Succeeded");
    }

    fn inst(hw: &str, ver: &str) -> InstalledDriver {
        InstalledDriver {
            device_name: "Dev".into(),
            hardware_ids: hw
                .split(';')
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect(),
            version: ver.into(),
            ..Default::default()
        }
    }

    #[test]
    fn matches_by_hardware_id() {
        let list = [
            inst("PCI\\VEN_8086&DEV_1234", "1.0.0.0"),
            inst("USB\\VID_1", "2.0"),
        ];
        let mut u = [DriverUpdate {
            hardware_id: "pci\\ven_8086&dev_1234".into(),
            new_version: "1.0.1.0".into(),
            ..Default::default()
        }];
        fill_current(&mut u, &list);
        assert_eq!(u[0].current_version, "1.0.0.0");
        assert_eq!(is_newer(&u[0]), Some(true));
    }

    #[test]
    fn matches_secondary_hardware_id() {
        let list = [inst(
            "PCI\\VEN_8086&DEV_7D67&SUBSYS_1;PCI\\VEN_8086&DEV_7D67",
            "31.0",
        )];
        let u = DriverUpdate {
            hardware_id: "PCI\\VEN_8086&DEV_7D67".into(),
            ..Default::default()
        };
        assert!(match_installed(&u, &list).is_some());
    }

    #[test]
    fn name_fallback_refuses_ambiguity() {
        let mut a = inst("", "1.0");
        a.device_name = "Intel(R) Graphics".into();
        let mut b = a.clone();
        b.version = "2.0".into();
        let u = DriverUpdate {
            device_name: "intel(r) graphics".into(),
            ..Default::default()
        };
        assert_eq!(
            match_installed(&u, std::slice::from_ref(&a)).map(|d| d.version.as_str()),
            Some("1.0")
        );
        assert!(match_installed(&u, &[a, b]).is_none());
    }

    #[test]
    fn status_flags_downgrades() {
        let mk = |cur: &str, new: &str| DriverUpdate {
            current_version: cur.into(),
            new_version: new.into(),
            ..Default::default()
        };
        assert_eq!(
            status(&mk("32.0.100.4404", "32.0.100.3053")),
            UpdateStatus::Older
        );
        assert_eq!(
            status(&mk("32.0.101.8331", "32.0.101.8331")),
            UpdateStatus::Same
        );
        assert_eq!(
            status(&mk("32.0.31041.1004", "32.0.31044.16")),
            UpdateStatus::Newer
        );
        assert_eq!(status(&mk("", "1.0")), UpdateStatus::Unknown);
    }

    #[test]
    fn gpu_updates_are_dropped() {
        let amd = DriverUpdate {
            title: "Advanced Micro Devices, Inc. Display Driver Update (32.0.31044.16)".into(),
            ..Default::default()
        };
        let by_class = DriverUpdate {
            title: "NVIDIA - something".into(),
            driver_class: "Display".into(),
            ..Default::default()
        };
        let by_device = DriverUpdate {
            title: "Intel Corporation - 31.0".into(),
            hardware_id: "PCI\\VEN_8086&DEV_7D67".into(),
            ..Default::default()
        };
        let npu = DriverUpdate {
            title: "Intel Corporation - ComputeAccelerator - 32.0.100.3053".into(),
            driver_class: "ComputeAccelerator".into(),
            ..Default::default()
        };
        let mut gpu_dev = inst("PCI\\VEN_8086&DEV_7D67", "31.0");
        gpu_dev.device_class = "DISPLAY".into();
        let mut v = vec![amd, by_class, by_device, npu];
        let dropped = drop_gpu_updates(&mut v, &[gpu_dev]);
        assert_eq!(dropped.len(), 3);
        assert_eq!(v.len(), 1);
        assert!(v[0].title.contains("ComputeAccelerator"));
    }

    #[test]
    fn in_use_marker_counts() {
        let before = windows_update_in_use();
        {
            let _a = WuInUse::new();
            assert!(windows_update_in_use());
        }
        assert_eq!(windows_update_in_use(), before);
    }

    #[test]
    fn no_match_without_ids() {
        let u = DriverUpdate::default();
        assert!(match_installed(&u, &[inst("", "1")]).is_none());
    }

    #[test]
    fn install_requires_selection() {
        // Fails before touching any system state (admin check or empty selection).
        let c = AtomicBool::new(false);
        let r = install_updates(&[], &c, &mut |_| {});
        assert!(r.is_err());
    }

    #[test]
    fn search_criteria_is_driver_only() {
        assert!(SEARCH_CRITERIA.contains("Type='Driver'"));
        assert!(SEARCH_CRITERIA.contains("IsInstalled=0"));
    }
}
