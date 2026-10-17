use eframe::egui::{self, Color32, Context};
use rustacer_core::bootperf::{self, BootReport};
use rustacer_core::cleaner::{self, Category, CleanReport, ScanResult};
use rustacer_core::config::{Config, Theme};
use rustacer_core::diskusage::{self, DiskReport, ScanProgress as DiskProgress};
use rustacer_core::drivehealth::{self, HealthReport};
use rustacer_core::drivers::backup::{self, BackupResult};
use rustacer_core::drivers::gpu::GpuVendor;
use rustacer_core::drivers::installed::{self, InstalledDriver};
use rustacer_core::drivers::restore::{self, RestoreOutcome};
use rustacer_core::drivers::wua::{self, DriverUpdate, InstallSummary, Progress};
use rustacer_core::metrics::{self, Sampler, Snapshot, SystemInfo};
use rustacer_core::processes::{ProcessInfo, ProcessMonitor};
use rustacer_core::services::{self, Action as SvcAction, ServiceInfo};
use rustacer_core::startup::{self, StartupEntry};
use rustacer_core::uninstaller::{self, AppEntry, WingetApp};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Startup,
    Cleaner,
    DiskAnalyzer,
    Health,
    Drivers,
    Services,
    Processes,
    Uninstaller,
    Resources,
    Settings,
}

impl Page {
    pub const ALL: [Page; 11] = [
        Page::Dashboard,
        Page::Startup,
        Page::Cleaner,
        Page::DiskAnalyzer,
        Page::Health,
        Page::Drivers,
        Page::Services,
        Page::Processes,
        Page::Uninstaller,
        Page::Resources,
        Page::Settings,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Page::Dashboard => "nav.dashboard",
            Page::Startup => "nav.startup",
            Page::Cleaner => "nav.cleaner",
            Page::DiskAnalyzer => "nav.disk",
            Page::Health => "nav.health",
            Page::Drivers => "nav.drivers",
            Page::Services => "nav.services",
            Page::Processes => "nav.processes",
            Page::Uninstaller => "nav.uninstaller",
            Page::Resources => "nav.resources",
            Page::Settings => "nav.settings",
        }
    }
}

/// Messages from worker threads to the UI.
pub enum Msg {
    Sample(Box<Snapshot>),
    SysInfo(Box<SystemInfo>),
    Startup(Vec<StartupEntry>),
    Services(Vec<ServiceInfo>),
    Procs(Vec<ProcessInfo>),
    Apps(Vec<AppEntry>),
    Winget(Vec<WingetApp>),
    WingetAvailable(bool),
    CleanScan(Vec<ScanResult>),
    CleanDone(CleanReport),
    DriversInstalled(Vec<InstalledDriver>),
    UpdateProgress(Progress),
    /// (offered non-GPU updates, titles of GPU updates that were left out)
    UpdatesFound(Result<(Vec<DriverUpdate>, Vec<String>), String>),
    InstallProgress(Progress),
    RestorePoint(RestoreOutcome),
    InstallDone(Result<InstallSummary, String>),
    BackupLine(String),
    BackupDone(Result<BackupResult, String>),
    RestorePoints(Result<Vec<restore::RestorePoint>, String>),
    DiskProgress(DiskProgress),
    DiskDone(Result<Box<DiskReport>, String>),
    /// Paths that were moved to the Recycle Bin from the disk analyzer.
    DiskRecycled(Vec<PathBuf>),
    Boot(Result<Box<BootReport>, String>),
    Health(Result<Box<HealthReport>, String>),
    Notice {
        error: bool,
        text: String,
    },
    /// Refresh a list after a mutation.
    Reload(Page),
}

/// Sender that also wakes the UI thread.
#[derive(Clone)]
pub struct Bus {
    tx: Sender<Msg>,
    ctx: Context,
}

impl Bus {
    pub fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }
    pub fn ok(&self, text: impl Into<String>) {
        self.send(Msg::Notice {
            error: false,
            text: text.into(),
        });
    }
    pub fn err(&self, text: impl Into<String>) {
        self.send(Msg::Notice {
            error: true,
            text: text.into(),
        });
    }
}

/// Everything that needs a confirmation dialog.
#[derive(Clone)]
pub enum Act {
    StartupSet(StartupEntry, bool),
    StartupDelete(StartupEntry),
    StartupAdd {
        name: String,
        exe: String,
        args: String,
    },
    Clean(Vec<Category>),
    Service(String, SvcAction),
    KillProcess(u32, String),
    KillTree(u32, String),
    Uninstall(AppEntry, bool),
    WingetUninstall(WingetApp),
    InstallDrivers(Vec<DriverUpdate>),
    RebootNow,
    BackupDrivers(PathBuf),
    DeleteRestorePoint(u32, String),
    /// Move paths picked in the disk analyzer to the Recycle Bin (scan root, paths).
    RecycleDisk(PathBuf, Vec<PathBuf>),
}

pub struct Confirm {
    pub title: String,
    pub body: String,
    pub warning: Option<String>,
    pub act: Act,
}

pub struct Sample {
    pub t: f64,
    pub cores: Vec<f32>,
    pub ram: f32,
    pub swap: f32,
    pub disk_r: f64,
    pub disk_w: f64,
    pub net_d: f64,
    pub net_u: f64,
}

#[derive(Default)]
pub struct CleanerState {
    pub results: Vec<(ScanResult, bool)>,
    pub scanning: bool,
    pub cleaning: bool,
    pub report: Option<CleanReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartupTab {
    #[default]
    Entries,
    Boot,
}

#[derive(Default)]
pub struct BootState {
    pub report: Option<BootReport>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct StartupState {
    pub tab: StartupTab,
    pub boot: BootState,
    pub entries: Vec<StartupEntry>,
    pub loading: bool,
    pub filter: String,
    pub new_name: String,
    pub new_exe: String,
    pub new_args: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DriverTab {
    #[default]
    Installed,
    Updates,
    Backup,
    Gpu,
    RestorePoints,
}

#[derive(Default)]
pub struct DriversState {
    pub tab: DriverTab,
    pub installed: Vec<InstalledDriver>,
    pub loading: bool,
    pub filter: String,
    pub only_old: bool,
    pub updates: Vec<(DriverUpdate, bool)>,
    pub show_all_updates: bool,
    pub gpu_skipped: Vec<String>,
    pub restore_points: Vec<restore::RestorePoint>,
    pub rp_loading: bool,
    pub rp_loaded: bool,
    pub rp_error: Option<String>,
    pub scanning: bool,
    pub scanned_once: bool,
    pub scan_progress: Option<Progress>,
    pub cancel: Arc<AtomicBool>,
    pub installing: bool,
    pub install_progress: Option<Progress>,
    pub summary: Option<InstallSummary>,
    pub restore_prompt: Option<String>,
    pub restore_tx: Option<Sender<bool>>,
    pub reboot_prompt: bool,
    pub backup_dir: String,
    pub backing_up: bool,
    pub backup_log: Vec<String>,
    pub backup_result: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiskTab {
    #[default]
    Folders,
    Files,
}

#[derive(Default)]
pub struct DiskState {
    pub root: String,
    pub tab: DiskTab,
    pub scanning: bool,
    pub progress: Option<DiskProgress>,
    pub cancel: Arc<AtomicBool>,
    pub report: Option<DiskReport>,
    pub error: Option<String>,
    pub expanded: HashSet<usize>,
    /// 0 = name, 1 = size, 2 = files.
    pub sort_col: usize,
    pub sort_asc: bool,
    /// Selected path and whether it is a folder.
    pub selected: Option<(PathBuf, bool)>,
    /// Items were recycled after the scan, so the numbers are out of date.
    pub stale: bool,
    pub recycling: bool,
}

#[derive(Default)]
pub struct HealthState {
    pub report: Option<HealthReport>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ServicesState {
    pub list: Vec<ServiceInfo>,
    pub loading: bool,
    pub filter: String,
    pub selected: Option<String>,
    pub loaded: bool,
}

#[derive(Default)]
pub struct ProcState {
    pub list: Vec<ProcessInfo>,
    pub filter: String,
    pub selected: Option<u32>,
    pub sort_col: usize,
    pub sort_desc: bool,
}

#[derive(Default)]
pub struct UninstState {
    pub list: Vec<AppEntry>,
    pub loading: bool,
    pub loaded: bool,
    pub filter: String,
    pub hide_system: bool,
    pub selected: Option<usize>,
    pub winget_ok: Option<bool>,
    pub winget_probed: bool,
    pub winget: Vec<WingetApp>,
    pub show_winget: bool,
    pub winget_selected: Option<usize>,
}

pub struct App {
    pub cfg: Config,
    pub page: Page,
    pub bus: Bus,
    rx: Receiver<Msg>,
    pub elevated: bool,
    pub notice: Option<(bool, String, Instant)>,
    pub confirm: Option<Confirm>,
    pub snap: Snapshot,
    pub info: Option<SystemInfo>,
    pub history: VecDeque<Sample>,
    pub started: Instant,
    pub res_range: f64,
    pub interval: Arc<AtomicU64>,
    pub procs_active: Arc<AtomicBool>,
    pub startup: StartupState,
    pub cleaner: CleanerState,
    pub drivers: DriversState,
    pub disk: DiskState,
    pub health: HealthState,
    pub services: ServicesState,
    pub procs: ProcState,
    pub uninst: UninstState,
    alert_times: HashMap<String, Instant>,
    applied_theme: Option<Theme>,
    applied_scale: Option<f32>,
    applied_motion: Option<bool>,
    pub self_registered: bool,
    /// `--smoke-test`: visit every page and driver tab, then exit.
    pub smoke: Option<(Option<Instant>, usize)>,
    /// `--screenshots <dir>` tour (see shots.rs).
    pub shots: Option<crate::shots::Shots>,
    /// Hide identifying details (PC name) - set during the screenshot tour.
    pub redact: bool,
}

/// A tab inside a page, for the smoke test and capture tours.
#[derive(Debug, Clone, Copy)]
pub enum Sub {
    None,
    Driver(DriverTab),
    Startup(StartupTab),
}

const SMOKE_STEPS: &[(Page, Sub)] = &[
    (Page::Dashboard, Sub::None),
    (Page::Startup, Sub::Startup(StartupTab::Entries)),
    (Page::Startup, Sub::Startup(StartupTab::Boot)),
    (Page::Cleaner, Sub::None),
    (Page::DiskAnalyzer, Sub::None),
    (Page::Health, Sub::None),
    (Page::Drivers, Sub::Driver(DriverTab::Installed)),
    (Page::Drivers, Sub::Driver(DriverTab::Updates)),
    (Page::Drivers, Sub::Driver(DriverTab::Backup)),
    (Page::Drivers, Sub::Driver(DriverTab::Gpu)),
    (Page::Drivers, Sub::Driver(DriverTab::RestorePoints)),
    (Page::Services, Sub::None),
    (Page::Processes, Sub::None),
    (Page::Uninstaller, Sub::None),
    (Page::Resources, Sub::None),
    (Page::Settings, Sub::None),
];

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        crate::theme::apply_sizing(&cc.egui_ctx);
        let (tx, rx) = channel();
        let bus = Bus {
            tx,
            ctx: cc.egui_ctx.clone(),
        };
        let interval = Arc::new(AtomicU64::new(cfg.refresh_interval_ms));
        let procs_active = Arc::new(AtomicBool::new(false));

        // Metrics sampler thread.
        {
            let bus = bus.clone();
            let interval = interval.clone();
            std::thread::spawn(move || {
                let mut s = Sampler::new();
                loop {
                    std::thread::sleep(Duration::from_millis(interval.load(Ordering::Relaxed)));
                    bus.send(Msg::Sample(Box::new(s.sample())));
                }
            });
        }
        // Process monitor thread (only refreshes while the Processes page is open).
        {
            let bus = bus.clone();
            let active = procs_active.clone();
            std::thread::spawn(move || {
                let mut m = ProcessMonitor::new();
                loop {
                    std::thread::sleep(Duration::from_millis(1500));
                    if active.load(Ordering::Relaxed) {
                        bus.send(Msg::Procs(m.refresh()));
                    }
                }
            });
        }
        // Static system info (WMI GPU query) off the UI thread.
        {
            let bus = bus.clone();
            std::thread::spawn(move || bus.send(Msg::SysInfo(Box::new(metrics::system_info()))));
        }

        let mut app = App {
            page: Page::Dashboard,
            bus,
            rx,
            elevated: rustacer_core::admin::is_elevated(),
            notice: None,
            confirm: None,
            snap: Snapshot::default(),
            info: None,
            history: VecDeque::new(),
            started: Instant::now(),
            res_range: 60.0,
            interval,
            procs_active,
            startup: StartupState::default(),
            cleaner: CleanerState::default(),
            drivers: DriversState::default(),
            health: HealthState::default(),
            disk: DiskState {
                sort_col: 1,
                ..Default::default()
            },
            services: ServicesState::default(),
            procs: ProcState {
                sort_col: 2,
                sort_desc: true,
                ..Default::default()
            },
            uninst: UninstState {
                hide_system: true,
                ..Default::default()
            },
            alert_times: HashMap::new(),
            applied_theme: None,
            applied_scale: None,
            applied_motion: None,
            self_registered: startup::is_self_registered(),
            shots: crate::shots::Shots::from_args(),
            redact: std::env::args().any(|a| a == "--screenshots"),
            smoke: std::env::args()
                .any(|a| a == "--smoke-test")
                .then_some((None, 0)),
            cfg,
        };
        app.drivers.backup_dir = dirs_documents();
        if app.cfg.check_drivers_on_startup {
            app.scan_driver_updates();
        }
        app
    }

    // ---------- plumbing ----------

    pub fn spawn(&self, f: impl FnOnce(Bus) + Send + 'static) {
        let bus = self.bus.clone();
        std::thread::spawn(move || f(bus));
    }

    pub fn notify(&mut self, error: bool, text: impl Into<String>) {
        self.notice = Some((error, text.into(), Instant::now()));
    }

    pub fn save_cfg(&mut self) {
        self.interval
            .store(self.cfg.refresh_interval_ms, Ordering::Relaxed);
        if let Err(e) = self.cfg.save() {
            error!("saving config failed: {e}");
            self.notify(true, format!("Could not save settings: {e}"));
        }
    }

    fn apply_theme(&mut self, ctx: &Context) {
        if self.applied_theme != Some(self.cfg.theme) {
            ctx.set_visuals(crate::theme::visuals(self.cfg.theme));
            self.applied_theme = Some(self.cfg.theme);
        }
        if self.applied_motion != Some(self.cfg.reduce_motion) {
            crate::theme::apply_motion(ctx, self.cfg.reduce_motion);
            self.applied_motion = Some(self.cfg.reduce_motion);
        }
        // UI scale: apply the setting, and save changes made with Ctrl+= / Ctrl+- / Ctrl+0.
        let live = ctx.zoom_factor();
        match self.applied_scale {
            Some(applied) if (applied - self.cfg.ui_scale).abs() > 0.001 => {
                ctx.set_zoom_factor(self.cfg.ui_scale);
                self.applied_scale = Some(self.cfg.ui_scale);
            }
            Some(applied) if (live - applied).abs() > 0.001 => {
                self.cfg.ui_scale = live.clamp(0.75, 2.0);
                ctx.set_zoom_factor(self.cfg.ui_scale);
                self.applied_scale = Some(self.cfg.ui_scale);
                self.save_cfg();
            }
            None => {
                ctx.set_zoom_factor(self.cfg.ui_scale);
                self.applied_scale = Some(self.cfg.ui_scale);
            }
            _ => {}
        }
    }

    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Sample(s) => self.on_sample(*s),
                Msg::SysInfo(i) => self.info = Some(*i),
                Msg::Startup(v) => {
                    self.startup.entries = v;
                    self.startup.loading = false;
                }
                Msg::Services(v) => {
                    self.services.list = v;
                    self.services.loading = false;
                    self.services.loaded = true;
                }
                Msg::Procs(v) => self.procs.list = v,
                Msg::Apps(v) => {
                    self.uninst.list = v;
                    self.uninst.loading = false;
                    self.uninst.loaded = true;
                }
                Msg::Winget(v) => self.uninst.winget = v,
                Msg::WingetAvailable(ok) => self.uninst.winget_ok = Some(ok),
                Msg::CleanScan(v) => {
                    self.cleaner.results = v
                        .into_iter()
                        .map(|r| {
                            let on = r.unavailable.is_none() && r.bytes > 0;
                            (r, on)
                        })
                        .collect();
                    self.cleaner.scanning = false;
                }
                Msg::CleanDone(r) => {
                    self.cleaner.cleaning = false;
                    self.cleaner.report = Some(r);
                    self.cleaner.results.clear();
                }
                Msg::DriversInstalled(v) => {
                    self.drivers.installed = v;
                    self.drivers.loading = false;
                }
                Msg::UpdateProgress(p) => self.drivers.scan_progress = Some(p),
                Msg::UpdatesFound(r) => {
                    self.drivers.scanning = false;
                    self.drivers.scan_progress = None;
                    self.drivers.scanned_once = true;
                    match r {
                        Ok((v, gpu)) => {
                            let n = v.len();
                            self.drivers.gpu_skipped = gpu;
                            self.drivers.updates = v.into_iter().map(|u| (u, false)).collect();
                            self.notify(
                                false,
                                format!("Windows Update offers {n} driver update(s)."),
                            );
                        }
                        Err(e) => self.notify(true, e),
                    }
                }
                Msg::InstallProgress(p) => self.drivers.install_progress = Some(p),
                Msg::RestorePoint(o) => match o {
                    RestoreOutcome::Created { .. } => {
                        self.notify(false, "System Restore accepted the restore point request.");
                    }
                    RestoreOutcome::Failed(why) => self.drivers.restore_prompt = Some(why),
                },
                Msg::InstallDone(r) => {
                    self.drivers.installing = false;
                    self.drivers.install_progress = None;
                    match r {
                        Ok(s) => {
                            self.drivers.reboot_prompt = false;
                            self.drivers.summary = Some(s);
                            self.drivers.updates.clear();
                        }
                        Err(e) => self.notify(true, e),
                    }
                }
                Msg::BackupLine(l) => self.drivers.backup_log.push(l),
                Msg::BackupDone(r) => {
                    self.drivers.backing_up = false;
                    self.drivers.backup_result = Some(match r {
                        Ok(b) => format!("Backup finished: {} driver package(s) exported (pnputil exit code {}).", b.exported, b.exit_code),
                        Err(e) => format!("Backup failed: {e}"),
                    });
                }
                Msg::RestorePoints(r) => {
                    self.drivers.rp_loading = false;
                    self.drivers.rp_loaded = true;
                    match r {
                        Ok(v) => {
                            self.drivers.restore_points = v;
                            self.drivers.rp_error = None;
                        }
                        Err(e) => self.drivers.rp_error = Some(e),
                    }
                }
                Msg::DiskProgress(p) => self.disk.progress = Some(p),
                Msg::DiskDone(r) => {
                    self.disk.scanning = false;
                    self.disk.progress = None;
                    match r {
                        Ok(rep) => {
                            self.disk.error = None;
                            self.disk.expanded = HashSet::from([0]);
                            self.disk.selected = None;
                            self.disk.stale = false;
                            self.disk.report = Some(*rep);
                        }
                        Err(e) => self.disk.error = Some(e),
                    }
                }
                Msg::DiskRecycled(paths) => {
                    self.disk.recycling = false;
                    if !paths.is_empty() {
                        self.disk.stale = true;
                        self.disk.selected = None;
                        if let Some(r) = &mut self.disk.report {
                            r.top_files
                                .retain(|f| !paths.iter().any(|p| f.path.starts_with(p)));
                        }
                    }
                }
                Msg::Health(r) => {
                    self.health.loading = false;
                    self.health.loaded = true;
                    match r {
                        Ok(rep) => {
                            self.health.error = None;
                            self.health.report = Some(*rep);
                        }
                        Err(e) => self.health.error = Some(e),
                    }
                }
                Msg::Boot(r) => {
                    self.startup.boot.loading = false;
                    self.startup.boot.loaded = true;
                    match r {
                        Ok(rep) => {
                            self.startup.boot.error = None;
                            self.startup.boot.report = Some(*rep);
                        }
                        Err(e) => self.startup.boot.error = Some(e),
                    }
                }
                Msg::Notice { error, text } => self.notify(error, text),
                Msg::Reload(Page::Drivers) if self.drivers.tab == DriverTab::RestorePoints => {
                    self.load_restore_points()
                }
                Msg::Reload(p) => self.load_page(p, true),
            }
        }
    }

    fn on_sample(&mut self, s: Snapshot) {
        let t = self.started.elapsed().as_secs_f64();
        self.history.push_back(Sample {
            t,
            cores: s.cpu_cores.clone(),
            ram: s.ram_percent(),
            swap: s.swap_percent(),
            disk_r: s.disk_read_bps,
            disk_w: s.disk_write_bps,
            net_d: s.net_down_bps,
            net_u: s.net_up_bps,
        });
        while self.history.front().is_some_and(|f| t - f.t > 1800.0) {
            self.history.pop_front();
        }
        if self.cfg.alerts_enabled {
            for (key, msg) in metrics::exceeded_alerts(
                &s,
                self.cfg.cpu_alert_percent,
                self.cfg.ram_alert_percent,
                self.cfg.disk_alert_percent,
            ) {
                let due = self
                    .alert_times
                    .get(&key)
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(300));
                if due {
                    self.alert_times.insert(key, Instant::now());
                    toast("Rustacer alert", &msg);
                }
            }
        }
        self.snap = s;
    }

    /// Kick off a (re)load when a page becomes visible.
    pub fn load_page(&mut self, p: Page, force: bool) {
        match p {
            Page::Startup
                if force || (self.startup.entries.is_empty() && !self.startup.loading) =>
            {
                self.startup.loading = true;
                self.spawn(|bus| bus.send(Msg::Startup(startup::list_entries())));
            }
            Page::Health if force || (!self.health.loaded && !self.health.loading) => {
                self.health.loading = true;
                self.spawn(|bus| {
                    bus.send(Msg::Health(
                        drivehealth::read_health()
                            .map(Box::new)
                            .map_err(|e| e.to_string()),
                    ));
                });
            }
            Page::Services if force || !self.services.loaded => {
                if !self.services.loading {
                    self.services.loading = true;
                    self.spawn(|bus| match services::list_services() {
                        Ok(v) => bus.send(Msg::Services(v)),
                        Err(e) => {
                            bus.err(format!("Could not list services: {e}"));
                            bus.send(Msg::Services(vec![]));
                        }
                    });
                }
            }
            Page::Uninstaller if force || !self.uninst.loaded => {
                if !self.uninst.loading {
                    self.uninst.loading = true;
                    self.spawn(|bus| bus.send(Msg::Apps(uninstaller::list_installed())));
                }
            }
            Page::Drivers
                if force || (self.drivers.installed.is_empty() && !self.drivers.loading) =>
            {
                self.drivers.loading = true;
                self.spawn(|bus| match installed::list_installed() {
                    Ok(v) => bus.send(Msg::DriversInstalled(v)),
                    Err(e) => {
                        bus.err(format!("Could not read installed drivers: {e}"));
                        bus.send(Msg::DriversInstalled(vec![]));
                    }
                });
            }
            _ => {}
        }
    }

    /// Switch to a tab inside the current page.
    pub fn show_sub(&mut self, sub: Sub) {
        match sub {
            Sub::None => {}
            Sub::Driver(t) => self.drivers.tab = t,
            Sub::Startup(t) => self.startup.tab = t,
        }
    }

    pub fn goto(&mut self, p: Page) {
        self.page = p;
        self.procs_active
            .store(p == Page::Processes, Ordering::Relaxed);
        self.load_page(p, false);
    }

    // ---------- confirmed actions ----------

    pub fn ask(&mut self, title: &str, body: String, warning: Option<String>, act: Act) {
        self.confirm = Some(Confirm {
            title: title.into(),
            body,
            warning,
            act,
        });
    }

    pub fn restart_as_admin(&mut self, ctx: &Context) {
        match rustacer_core::admin::restart_as_admin() {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => self.notify(true, e.to_string()),
        }
    }

    pub fn load_restore_points(&mut self) {
        if self.drivers.rp_loading {
            return;
        }
        self.drivers.rp_loading = true;
        self.spawn(|bus| {
            bus.send(Msg::RestorePoints(
                restore::list_restore_points().map_err(|e| e.to_string()),
            ))
        });
    }

    pub fn scan_driver_updates(&mut self) {
        if self.drivers.scanning {
            return;
        }
        self.drivers.scanning = true;
        self.drivers.cancel.store(false, Ordering::Relaxed);
        let cancel = self.drivers.cancel.clone();
        self.spawn(move |bus| {
            let installed = installed::list_installed().unwrap_or_default();
            let b2 = bus.clone();
            let r = wua::search_driver_updates(&cancel, &mut |p| b2.send(Msg::UpdateProgress(p)));
            bus.send(Msg::UpdatesFound(match r {
                Ok(mut v) => {
                    wua::fill_current(&mut v, &installed);
                    let gpu = wua::drop_gpu_updates(&mut v, &installed);
                    Ok((v, gpu))
                }
                Err(e) => Err(e.to_string()),
            }));
        });
    }

    /// Scan the folder in `disk.root` on a worker thread (read-only).
    pub fn scan_disk(&mut self) {
        if self.disk.scanning {
            return;
        }
        let root = PathBuf::from(self.disk.root.trim());
        if root.as_os_str().is_empty() {
            self.notify(true, "Choose a drive or folder to scan first.");
            return;
        }
        self.disk.scanning = true;
        self.disk.error = None;
        self.disk.progress = None;
        self.disk.cancel.store(false, Ordering::Relaxed);
        let cancel = self.disk.cancel.clone();
        self.spawn(move |bus| {
            let b2 = bus.clone();
            let r = diskusage::scan(&root, &cancel, &mut |p| {
                b2.send(Msg::DiskProgress(p.clone()))
            });
            bus.send(Msg::DiskDone(r.map(Box::new).map_err(|e| e.to_string())));
        });
    }

    /// Read the boot-performance log (admin only) on a worker thread.
    pub fn load_boot(&mut self) {
        if self.startup.boot.loading {
            return;
        }
        self.startup.boot.loading = true;
        self.spawn(|bus| {
            bus.send(Msg::Boot(
                bootperf::read_boot_report()
                    .map(Box::new)
                    .map_err(|e| e.to_string()),
            ));
        });
    }

    pub fn scan_cleaner(&mut self) {
        self.cleaner.scanning = true;
        self.cleaner.report = None;
        self.spawn(|bus| {
            let cancel = AtomicBool::new(false);
            let res: Vec<ScanResult> = cleaner::system_targets()
                .iter()
                .map(|t| cleaner::scan_target(t, &cancel))
                .collect();
            bus.send(Msg::CleanScan(res));
        });
    }

    pub fn run_act(&mut self, act: Act) {
        match act {
            Act::StartupSet(e, on) => self.spawn(move |bus| {
                match startup::set_enabled(&e, on) {
                    Ok(()) => bus.ok(format!(
                        "'{}' {}.",
                        e.name,
                        if on { "enabled" } else { "disabled" }
                    )),
                    Err(err) => bus.err(err.to_string()),
                }
                bus.send(Msg::Reload(Page::Startup));
            }),
            Act::StartupDelete(e) => self.spawn(move |bus| {
                match startup::delete_entry(&e) {
                    Ok(()) => bus.ok(format!("'{}' deleted.", e.name)),
                    Err(err) => bus.err(err.to_string()),
                }
                bus.send(Msg::Reload(Page::Startup));
            }),
            Act::StartupAdd { name, exe, args } => self.spawn(move |bus| {
                match startup::add_user_entry(&name, &exe, &args) {
                    Ok(()) => bus.ok(format!("Startup entry '{name}' added.")),
                    Err(err) => bus.err(err.to_string()),
                }
                bus.send(Msg::Reload(Page::Startup));
            }),
            Act::Clean(mut cats) => {
                if (self.drivers.scanning || self.drivers.installing)
                    && cats.contains(&Category::WindowsUpdateCache)
                {
                    cats.retain(|c| *c != Category::WindowsUpdateCache);
                    self.notify(
                        true,
                        "Skipped the Windows Update cache: the driver updater is using Windows Update.",
                    );
                }
                self.cleaner.cleaning = true;
                self.spawn(move |bus| {
                    let cancel = AtomicBool::new(false);
                    let mut total = CleanReport::default();
                    for t in cleaner::system_targets()
                        .iter()
                        .filter(|t| cats.contains(&t.category))
                    {
                        match cleaner::clean_target(t, &cancel) {
                            Ok(r) => {
                                total.freed_bytes += r.freed_bytes;
                                total.files_deleted += r.files_deleted;
                                total.skipped.extend(r.skipped);
                            }
                            Err(e) => bus.err(format!("{}: {e}", t.category.label())),
                        }
                    }
                    bus.send(Msg::CleanDone(total));
                });
            }
            Act::Service(name, action) => {
                let busy = self.drivers.scanning || self.drivers.installing;
                self.spawn(move |bus| {
                    match services::apply(&name, action, busy) {
                        Ok(()) => bus.ok(format!("Service '{name}': done.")),
                        Err(e) => bus.err(e.to_string()),
                    }
                    bus.send(Msg::Reload(Page::Services));
                });
            }
            Act::KillProcess(pid, name) => {
                self.spawn(move |bus| match ProcessMonitor::new().kill(pid) {
                    Ok(()) => bus.ok(format!("Ended '{name}' ({pid}).")),
                    Err(e) => bus.err(e.to_string()),
                })
            }
            Act::KillTree(pid, name) => {
                self.spawn(move |bus| match ProcessMonitor::new().kill_tree(pid) {
                    Ok(n) => bus.ok(format!("Ended '{name}' and its tree ({n} processes).")),
                    Err(e) => bus.err(e.to_string()),
                })
            }
            Act::Uninstall(app, quiet) => {
                self.spawn(move |bus| match uninstaller::uninstall(&app, quiet) {
                    Ok(()) => bus.ok(format!("Uninstaller for '{}' launched.", app.name)),
                    Err(e) => bus.err(e.to_string()),
                })
            }
            Act::WingetUninstall(w) => {
                self.spawn(move |bus| match uninstaller::winget_uninstall(&w.id) {
                    Ok(()) => bus.ok(format!("winget uninstall for '{}' launched.", w.name)),
                    Err(e) => bus.err(e.to_string()),
                })
            }
            Act::RebootNow => self.spawn(|bus| match wua::reboot_now() {
                Ok(()) => bus.ok("Restarting in 10 seconds. Run 'shutdown /a' to abort."),
                Err(e) => bus.err(e.to_string()),
            }),
            Act::BackupDrivers(dir) => {
                self.drivers.backing_up = true;
                self.drivers.backup_log.clear();
                self.drivers.backup_result = None;
                self.spawn(move |bus| {
                    let b2 = bus.clone();
                    let r =
                        backup::backup_drivers(&dir, |l| b2.send(Msg::BackupLine(l.to_string())));
                    bus.send(Msg::BackupDone(r.map_err(|e| e.to_string())));
                });
            }
            Act::InstallDrivers(list) => self.start_install(list),
            Act::RecycleDisk(root, paths) => {
                self.disk.recycling = true;
                self.spawn(move |bus| {
                    info!(
                        "recycling {} item(s) picked in the disk analyzer",
                        paths.len()
                    );
                    match diskusage::recycle(&root, &paths) {
                        Ok(n) => {
                            bus.ok(format!("Moved {n} item(s) to the Recycle Bin."));
                            bus.send(Msg::DiskRecycled(paths));
                        }
                        Err(e) => {
                            bus.err(e.to_string());
                            bus.send(Msg::DiskRecycled(vec![]));
                        }
                    }
                });
            }
            Act::DeleteRestorePoint(seq, desc) => self.spawn(move |bus| {
                match restore::delete_restore_point(seq) {
                    Ok(()) => bus.ok(format!("Deleted restore point #{seq} ({desc}).")),
                    Err(e) => bus.err(e.to_string()),
                }
                bus.send(Msg::Reload(Page::Drivers));
            }),
        }
    }

    fn start_install(&mut self, list: Vec<DriverUpdate>) {
        self.drivers.installing = true;
        self.drivers.summary = None;
        self.drivers.cancel.store(false, Ordering::Relaxed);
        let cancel = self.drivers.cancel.clone();
        let (dtx, drx) = channel::<bool>();
        self.drivers.restore_tx = Some(dtx);
        self.spawn(move |bus| {
            // 1. restore point first
            let outcome = restore::create_restore_point("Rustacer: before driver update");
            let seq = match &outcome {
                RestoreOutcome::Created { sequence } => Some(*sequence),
                RestoreOutcome::Failed(_) => None,
            };
            let failed = seq.is_none();
            bus.send(Msg::RestorePoint(outcome));
            if failed {
                // Wait for the user's explicit decision; never proceed silently.
                if !matches!(drx.recv(), Ok(true)) {
                    bus.send(Msg::InstallDone(Err(
                        "Installation cancelled before anything was installed.".into(),
                    )));
                    return;
                }
            }
            // 2. install exactly the selected updates
            let ids: Vec<String> = list.iter().map(|u| u.update_id.clone()).collect();
            info!(
                "user confirmed installation of {} driver update(s)",
                ids.len()
            );
            let b2 = bus.clone();
            let r = wua::install_updates(&ids, &cancel, &mut |p| b2.send(Msg::InstallProgress(p)));
            if let Some(s) = seq {
                restore::finish_restore_point(s);
            }
            bus.send(Msg::InstallDone(r.map_err(|e| e.to_string())));
        });
    }

    /// F11 toggles fullscreen; Esc leaves it (when no dialog is open).
    fn window_keys(&mut self, ctx: &Context) {
        let (f11, esc, fullscreen) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::F11),
                i.key_pressed(egui::Key::Escape),
                i.viewport().fullscreen.unwrap_or(false),
            )
        });
        if f11 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        } else if esc && fullscreen && self.confirm.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        // Ctrl+1 .. Ctrl+9: jump to a page (same order as the sidebar).
        if self.confirm.is_none() {
            const KEYS: [egui::Key; 9] = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
                egui::Key::Num9,
            ];
            let hit = ctx.input_mut(|i| {
                KEYS.iter()
                    .position(|k| i.consume_key(egui::Modifiers::COMMAND, *k))
            });
            if let Some(n) = hit.filter(|n| *n < Page::ALL.len()) {
                self.goto(Page::ALL[n]);
            }
            // Ctrl+PageDown / Ctrl+PageUp: next / previous page, wrapping around.
            let (next, prev) = ctx.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::COMMAND, egui::Key::PageDown),
                    i.consume_key(egui::Modifiers::COMMAND, egui::Key::PageUp),
                )
            });
            if next || prev {
                let n = Page::ALL.len();
                let cur = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0);
                let to = if next {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                self.goto(Page::ALL[to]);
            }
        }
    }

    fn smoke_step(&mut self, ctx: &Context) {
        let Some((started, step)) = self.smoke else {
            return;
        };
        let Some(t0) = started else {
            let (page, tab) = SMOKE_STEPS[step];
            info!(
                "smoke test: showing {page:?} {tab:?} (loaded so far: {} startup, {} drivers, {} services, {} processes, {} apps, {} history samples)",
                self.startup.entries.len(),
                self.drivers.installed.len(),
                self.services.list.len(),
                self.procs.list.len(),
                self.uninst.list.len(),
                self.history.len()
            );
            self.goto(page);
            self.show_sub(tab);
            self.smoke = Some((Some(Instant::now()), step));
            ctx.request_repaint();
            return;
        };
        // Give background loads time to finish and their results time to be drawn.
        if t0.elapsed() >= Duration::from_millis(2500) {
            if step + 1 == SMOKE_STEPS.len() {
                info!(
                    "smoke test passed: {} steps rendered without a panic",
                    SMOKE_STEPS.len()
                );
                self.smoke = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            self.smoke = Some((None, step + 1));
        }
        ctx.request_repaint();
    }

    pub fn open_vendor(&mut self, v: GpuVendor) {
        self.spawn(move |bus| {
            if let Err(e) = rustacer_core::drivers::gpu::open_official_page(v) {
                bus.err(e.to_string());
            }
        });
    }

    // ---------- confirmation dialog ----------

    fn confirm_dialog(&mut self, ctx: &Context) {
        let Some(c) = &self.confirm else { return };
        let (title, body, warning) = (c.title.clone(), c.body.clone(), c.warning.clone());
        let mut decision: Option<bool> = None;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(body);
                if let Some(w) = warning {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::from_rgb(0xf5, 0xa6, 0x23), w);
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add(crate::widgets::primary_btn(ui, "Confirm")).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                if let Some(c) = self.confirm.take() {
                    info!("user confirmed: {}", c.title);
                    self.run_act(c.act);
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn driver_dialogs(&mut self, ctx: &Context) {
        if let Some(why) = self.drivers.restore_prompt.clone() {
            let mut choice = None;
            egui::Window::new("No restore point")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(460.0);
                    ui.colored_label(
                        Color32::from_rgb(0xe5, 0x48, 0x4d),
                        format!("A System Restore point could not be created: {why}."),
                    );
                    ui.label("If the driver install goes wrong you will not be able to roll back with System Restore.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel installation").clicked() {
                            choice = Some(false);
                        }
                        if ui.button("Continue without restore point").clicked() {
                            choice = Some(true);
                        }
                    });
                });
            if let Some(c) = choice {
                self.drivers.restore_prompt = None;
                if let Some(tx) = &self.drivers.restore_tx {
                    let _ = tx.send(c);
                }
            }
        }
    }

    fn top_notice(&mut self, ui: &mut egui::Ui) {
        if let Some((err, text, at)) = &self.notice {
            if at.elapsed() > Duration::from_secs(12) {
                self.notice = None;
                return;
            }
            let (err, text) = (*err, text.clone());
            let color = if err {
                Color32::from_rgb(0xe5, 0x48, 0x4d)
            } else {
                Color32::from_rgb(0x3d, 0xb8, 0x6b)
            };
            let mut close = false;
            ui.horizontal(|ui| {
                ui.colored_label(color, if err { "Error:" } else { "OK:" });
                ui.label(text);
                if ui.small_button("x").clicked() {
                    close = true;
                }
            });
            ui.separator();
            if close {
                self.notice = None;
            }
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
    }
}

fn dirs_documents() -> String {
    std::env::var("USERPROFILE")
        .map(|p| format!("{p}\\Documents\\Rustacer-DriverBackup"))
        .unwrap_or_default()
}

pub fn toast(title: &str, body: &str) {
    let (t, b) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let _ = tauri_winrt_notification::Toast::new(
            tauri_winrt_notification::Toast::POWERSHELL_APP_ID,
        )
        .title(&t)
        .text1(&b)
        .show();
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain();
        self.apply_theme(&ctx);
        crate::pages::sidebar(self, ui, &ctx);
        egui::CentralPanel::default().show(ui, |ui| {
            self.top_notice(ui);
            crate::pages::show(self, ui, &ctx);
        });
        self.smoke_step(&ctx);
        crate::shots::tick(self, &ctx);
        self.window_keys(&ctx);
        self.confirm_dialog(&ctx);
        self.driver_dialogs(&ctx);
        // History/gauges move on their own via sampler wakeups; keep a slow tick for notices.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}
