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
use rustacer_core::netconn::{self, Connection};
use rustacer_core::processes::{ProcessInfo, ProcessMonitor};
use rustacer_core::report::{self, Format as ReportFormat};
use rustacer_core::services::{self, Action as SvcAction, ServiceInfo};
use rustacer_core::startup::{self, StartupEntry};
use rustacer_core::tray::{self, Tray, TrayEvent};
use rustacer_core::uninstaller::{self, AppEntry, UpgradeList, WingetApp, WingetUpgrade};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
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
    AppUpdates,
    Services,
    Processes,
    Uninstaller,
    Resources,
    Settings,
}

impl Page {
    pub const ALL: [Page; 12] = [
        Page::Dashboard,
        Page::Startup,
        Page::Cleaner,
        Page::DiskAnalyzer,
        Page::Health,
        Page::Drivers,
        Page::AppUpdates,
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
            Page::AppUpdates => "nav.updates",
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
    Net(Result<Vec<Connection>, String>),
    ReportProgress(String),
    ReportDone(Result<PathBuf, String>),
    Upgrades(Result<Box<UpgradeList>, String>),
    UpgradeProgress(String),
    /// (package id, outcome text, winget exit code) for every package that was attempted.
    UpgradeDone(Vec<(String, String, Option<u32>)>),
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
    pub ctx: Context,
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
    WingetUpgrade(Vec<WingetUpgrade>),
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

pub struct ReportState {
    pub open: bool,
    pub format: ReportFormat,
    /// Off by default: the PC name and user name are redacted.
    pub include_identity: bool,
    pub include_services: bool,
    pub running: bool,
    pub progress: String,
    pub cancel: Arc<AtomicBool>,
}

impl Default for ReportState {
    fn default() -> Self {
        Self {
            open: false,
            format: ReportFormat::Html,
            include_identity: false,
            include_services: false,
            running: false,
            progress: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcTab {
    #[default]
    Processes,
    Network,
}

#[derive(Default)]
pub struct NetState {
    pub list: Vec<Connection>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub filter: String,
    pub selected: Option<Connection>,
    /// 0 process, 1 PID, 2 protocol, 3 local, 4 remote, 5 state.
    pub sort_col: usize,
    pub sort_desc: bool,
}

#[derive(Default)]
pub struct ProcState {
    pub tab: ProcTab,
    pub net: NetState,
    pub list: Vec<ProcessInfo>,
    pub filter: String,
    pub selected: Option<u32>,
    pub sort_col: usize,
    pub sort_desc: bool,
}

#[derive(Default)]
pub struct UpdatesState {
    pub items: Vec<(WingetUpgrade, bool)>,
    pub excluded: Vec<(WingetUpgrade, &'static str)>,
    pub note: Option<String>,
    pub error: Option<String>,
    pub checking: bool,
    pub checked: bool,
    pub running: bool,
    pub progress: String,
    pub cancel: Arc<AtomicBool>,
    pub results: Vec<(String, String)>,
}

#[derive(Default)]
pub struct UninstState {
    pub updates: UpdatesState,
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
    /// True only while the Network tab is on screen and the window is visible.
    pub net_active: Arc<AtomicBool>,
    pub startup: StartupState,
    pub cleaner: CleanerState,
    pub drivers: DriversState,
    pub disk: DiskState,
    pub health: HealthState,
    pub report: ReportState,
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
    /// Notification-area icon, present while a tray option is on.
    tray: Option<Tray>,
    /// Set by the tray's Quit item so the close request is not turned into "hide".
    quit_requested: Arc<AtomicBool>,
    /// Tells the background threads to stop (set on exit).
    shutdown: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    tray_tip_at: Instant,
    tray_hint_shown: bool,
    /// Launched from the Windows Run key with `--tray`: hide once the tray icon exists,
    /// giving up (and staying visible) after this deadline.
    start_hidden_until: Option<Instant>,
    /// `--tray-test`: scripted hide / show / quit check (see `tray_test_step`).
    tray_test: Option<TrayTest>,
    /// `--soak-test <seconds> [folder]`: long run on the Network tab with a disk scan.
    soak: Option<Soak>,
}

/// Leak and busy-loop check: scan a folder (read-only), then sit on the Network tab,
/// visiting the Disk Analyzer once a minute, and quit after the given time.
pub struct Soak {
    t0: Instant,
    secs: u64,
    folder: String,
    started_scan: bool,
    last_switch: Instant,
}

impl Soak {
    fn from_args() -> Option<Self> {
        let args: Vec<String> = std::env::args().collect();
        let i = args.iter().position(|a| a == "--soak-test")?;
        let secs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(300);
        let folder = args
            .get(i + 2)
            .cloned()
            .or_else(|| std::env::var("ProgramFiles").ok())
            .unwrap_or_else(|| r"C:\Program Files".into());
        Some(Self {
            t0: Instant::now(),
            secs,
            folder,
            started_scan: false,
            last_switch: Instant::now(),
        })
    }
}

/// Scripted self-test of the tray round trip. Writes PASS/FAIL lines to a text file.
pub struct TrayTest {
    t0: Instant,
    step: usize,
    log: Vec<String>,
    failed: bool,
}

/// A tab inside a page, for the smoke test and capture tours.
#[derive(Debug, Clone, Copy)]
pub enum Sub {
    None,
    Driver(DriverTab),
    Startup(StartupTab),
    Procs(ProcTab),
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
    (Page::Processes, Sub::Procs(ProcTab::Processes)),
    (Page::Processes, Sub::Procs(ProcTab::Network)),
    (Page::AppUpdates, Sub::None),
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
        let net_active = Arc::new(AtomicBool::new(false));

        let shutdown = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        // Metrics sampler thread.
        {
            let bus = bus.clone();
            let interval = interval.clone();
            let stop = shutdown.clone();
            threads.extend(
                std::thread::Builder::new()
                    .name("rustacer-sampler".into())
                    .spawn(move || {
                        let mut s = Sampler::new();
                        while nap(&stop, interval.load(Ordering::Relaxed)) {
                            bus.send(Msg::Sample(Box::new(s.sample())));
                        }
                    })
                    .ok(),
            );
        }
        // Process monitor thread (only refreshes while the Processes page is open).
        {
            let bus = bus.clone();
            let active = procs_active.clone();
            let stop = shutdown.clone();
            threads.extend(
                std::thread::Builder::new()
                    .name("rustacer-procs".into())
                    .spawn(move || {
                        let mut m = ProcessMonitor::new();
                        while nap(&stop, 1500) {
                            if active.load(Ordering::Relaxed) {
                                bus.send(Msg::Procs(m.refresh()));
                            }
                        }
                    })
                    .ok(),
            );
        }
        // Connection table: refreshed every 2 s, and only while the Network tab is showing.
        {
            let bus = bus.clone();
            let active = net_active.clone();
            let stop = shutdown.clone();
            threads.extend(
                std::thread::Builder::new()
                    .name("rustacer-net".into())
                    .spawn(move || {
                        while nap(&stop, 2000) {
                            if active.load(Ordering::Relaxed) {
                                bus.send(Msg::Net(
                                    netconn::read_connections().map_err(|e| e.to_string()),
                                ));
                            }
                        }
                    })
                    .ok(),
            );
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
            net_active,
            startup: StartupState::default(),
            cleaner: CleanerState::default(),
            drivers: DriversState::default(),
            health: HealthState::default(),
            report: ReportState::default(),
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
            tray: None,
            quit_requested: Arc::new(AtomicBool::new(false)),
            shutdown,
            threads,
            tray_tip_at: Instant::now(),
            tray_hint_shown: false,
            start_hidden_until: None,
            tray_test: None,
            soak: Soak::from_args(),
            smoke: std::env::args()
                .any(|a| a == "--smoke-test")
                .then_some((None, 0)),
            cfg,
        };
        let capture_run = app.shots.is_some() || app.smoke.is_some() || app.soak.is_some();
        if capture_run {
            // Test runs never use the tray and must always be able to close.
            app.cfg.close_to_tray = false;
            app.cfg.start_minimized_to_tray = false;
            app.quit_requested.store(true, Ordering::Relaxed);
        }
        // `--tray-test` turns the tray on for this run only (nothing is saved).
        if !capture_run && std::env::args().any(|a| a == "--tray-test") {
            app.cfg.close_to_tray = true;
            app.tray_test = Some(TrayTest {
                t0: Instant::now(),
                step: 0,
                log: vec![],
                failed: false,
            });
        }
        app.sync_tray();
        // A login launch from the Run key with `--tray` starts hidden (if the user turned that on).
        if !capture_run
            && app.cfg.start_minimized_to_tray
            && app.self_registered
            && std::env::args().any(|a| a == startup::TRAY_ARG)
        {
            app.start_hidden_until = Some(Instant::now() + Duration::from_secs(60));
        }
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

    // ---------- system tray ----------

    /// Create or remove the tray icon so it matches the settings.
    pub fn sync_tray(&mut self) {
        let want = self.cfg.tray_enabled();
        if want && self.tray.is_none() {
            let ctx = self.bus.ctx.clone();
            let quit = self.quit_requested.clone();
            let tip =
                tray::format_tooltip(self.snap.cpu_total, self.snap.ram_used, self.snap.ram_total);
            match Tray::start(&tip, move |ev| match ev {
                TrayEvent::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
                TrayEvent::Quit => {
                    quit.store(true, Ordering::Relaxed);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                }
            }) {
                Ok(t) => {
                    info!("tray icon created");
                    self.tray = Some(t);
                }
                Err(e) => {
                    error!("tray icon failed: {e}");
                    self.notify(true, e.to_string());
                    // Do not leave the user with a setting that cannot work.
                    self.cfg.close_to_tray = false;
                    self.cfg.start_minimized_to_tray = false;
                }
            }
        } else if !want && self.tray.is_some() {
            info!("tray icon removed");
            self.tray = None; // dropping stops the tray thread
        }
    }

    fn tray_ready(&self) -> bool {
        self.tray.as_ref().is_some_and(|t| t.is_shown())
    }

    /// The window's close button hides it instead when "Close to tray" is on.
    fn handle_close(&mut self, ctx: &Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.quit_requested.load(Ordering::Relaxed) {
            return; // a real quit: let it close
        }
        if self.cfg.close_to_tray && self.tray_ready() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            info!("window hidden to the tray");
            if !self.tray_hint_shown {
                self.tray_hint_shown = true;
                toast(
                    "Rustacer is still running",
                    "It is in the notification area. Right-click the icon and choose Quit to exit.",
                );
            }
        }
    }

    /// Start hidden for a login launch, once the tray icon is really there.
    fn handle_start_hidden(&mut self, ctx: &Context) {
        let Some(deadline) = self.start_hidden_until else {
            return;
        };
        if self.tray_ready() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            info!("started hidden in the tray");
            self.start_hidden_until = None;
        } else if Instant::now() > deadline {
            error!("no tray icon after 60 s; keeping the window visible");
            self.start_hidden_until = None;
        }
    }

    fn update_tray_tooltip(&mut self) {
        if let Some(t) = &self.tray {
            if self.tray_tip_at.elapsed() >= Duration::from_secs(2) {
                self.tray_tip_at = Instant::now();
                t.set_tooltip(&tray::format_tooltip(
                    self.snap.cpu_total,
                    self.snap.ram_used,
                    self.snap.ram_total,
                ));
            }
        }
    }

    /// `--tray-test`: close -> hidden, tray Open -> visible, close -> hidden, tray Quit -> exit.
    fn tray_test_step(&mut self, ctx: &Context) {
        let Some(mut t) = self.tray_test.take() else {
            return;
        };
        ctx.request_repaint();
        let secs = t.t0.elapsed().as_secs_f32();
        let visible = tray::window_visible("Rustacer");
        let check = |t: &mut TrayTest, name: &str, ok: bool| {
            t.log
                .push(format!("{} {name}", if ok { "PASS" } else { "FAIL" }));
            t.failed |= !ok;
        };
        match (t.step, secs) {
            (0, s) if s >= 1.5 => {
                check(
                    &mut t,
                    "tray icon is in the notification area",
                    self.tray_ready(),
                );
                check(&mut t, "window visible at start", visible == Some(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close); // like the X button
                t.step = 1;
            }
            (1, s) if s >= 3.0 => {
                check(
                    &mut t,
                    "close button hid the window",
                    visible == Some(false),
                );
                if let Some(tr) = &self.tray {
                    tr.simulate(TrayEvent::Open);
                }
                t.step = 2;
            }
            (2, s) if s >= 4.5 => {
                check(
                    &mut t,
                    "tray Open showed the window again",
                    visible == Some(true),
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                t.step = 3;
            }
            (3, s) if s >= 6.0 => {
                check(
                    &mut t,
                    "second close hid the window",
                    visible == Some(false),
                );
                // Hidden: frames must still run (alerts depend on them).
                check(
                    &mut t,
                    "UI frames kept running while hidden (the script advanced)",
                    true,
                );
                if let Some(tr) = &self.tray {
                    tr.simulate(TrayEvent::Quit);
                }
                t.step = 4;
            }
            (4, s) if s >= 12.0 => {
                // Quit did not end the process.
                check(&mut t, "tray Quit ended the program", false);
                Self::finish_tray_test(&t);
                std::process::exit(2);
            }
            _ => {}
        }
        if t.step == 4 {
            // Written before the quit request is processed; the process exit is checked by
            // whoever launched the test.
            Self::write_tray_test(&t, "QUIT-REQUESTED");
        }
        self.tray_test = Some(t);
    }

    fn write_tray_test(t: &TrayTest, tail: &str) {
        let mut text = t.log.join("\n");
        text.push('\n');
        text.push_str(tail);
        text.push('\n');
        let _ = std::fs::write(std::env::temp_dir().join("rustacer-tray-test.txt"), text);
    }

    fn finish_tray_test(t: &TrayTest) {
        Self::write_tray_test(
            t,
            if t.failed {
                "RESULT: FAIL"
            } else {
                "RESULT: PASS"
            },
        );
    }

    pub fn notify(&mut self, error: bool, text: impl Into<String>) {
        self.notice = Some((error, text.into(), Instant::now()));
    }

    pub fn save_cfg(&mut self) {
        self.interval
            .store(self.cfg.refresh_interval_ms, Ordering::Relaxed);
        self.sync_tray();
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
                Msg::ReportProgress(p) => self.report.progress = p,
                Msg::ReportDone(r) => {
                    self.report.running = false;
                    self.report.progress.clear();
                    match r {
                        Ok(p) => {
                            self.notify(false, format!("System report saved to {}", p.display()))
                        }
                        Err(e) => self.notify(true, e),
                    }
                }
                Msg::Net(r) => {
                    let n = &mut self.procs.net;
                    n.loading = false;
                    n.loaded = true;
                    match r {
                        Ok(v) => {
                            n.error = None;
                            n.list = v;
                        }
                        Err(e) => n.error = Some(e),
                    }
                }
                Msg::Upgrades(r) => {
                    let u = &mut self.uninst.updates;
                    u.checking = false;
                    u.checked = true;
                    match r {
                        Ok(l) => {
                            u.error = None;
                            u.note = l.note;
                            u.excluded = l.excluded;
                            u.items = l.items.into_iter().map(|i| (i, false)).collect();
                        }
                        Err(e) => u.error = Some(e),
                    }
                }
                Msg::UpgradeProgress(p) => self.uninst.updates.progress = p,
                Msg::UpgradeDone(results) => {
                    let u = &mut self.uninst.updates;
                    u.running = false;
                    u.progress.clear();
                    // Packages that went through are no longer pending.
                    let done: Vec<&str> = results
                        .iter()
                        .filter(|(_, r, _)| r.starts_with("Finished"))
                        .map(|(id, _, _)| id.as_str())
                        .collect();
                    u.items.retain(|(i, _)| !done.contains(&i.id.as_str()));
                    // winget can never upgrade these installs: move them to the left-out list
                    // so they are not offered again.
                    let stuck: Vec<&str> = results
                        .iter()
                        .filter(|(_, _, c)| *c == Some(uninstaller::EXIT_TECHNOLOGY_MISMATCH))
                        .map(|(id, _, _)| id.as_str())
                        .collect();
                    let (moved, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut u.items)
                        .into_iter()
                        .partition(|(i, _)| stuck.contains(&i.id.as_str()));
                    u.items = kept;
                    u.excluded.extend(
                        moved
                            .into_iter()
                            .map(|(i, _)| (i, uninstaller::TECHNOLOGY_MISMATCH)),
                    );
                    u.results = results.into_iter().map(|(id, r, _)| (id, r)).collect();
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
        self.update_tray_tooltip();
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
            Sub::Procs(t) => self.procs.tab = t,
        }
    }

    pub fn goto(&mut self, p: Page) {
        self.page = p;
        self.procs_active
            .store(p == Page::Processes, Ordering::Relaxed);
        self.net_active.store(false, Ordering::Relaxed);
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
            Ok(()) => {
                // A real exit: "Close to tray" must not keep this copy running next to
                // the elevated one (two processes, two tray icons).
                self.quit_requested.store(true, Ordering::Relaxed);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close)
            }
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

    /// Open the export options dialog (the choices start private: identity redacted).
    pub fn open_report_dialog(&mut self) {
        if self.report.running {
            return;
        }
        self.report.include_identity = false;
        self.report.open = true;
    }

    /// Ask where to save, then collect and write the report on a worker thread.
    fn start_report(&mut self) {
        let (name, label, ext) = match self.report.format {
            ReportFormat::Html => ("rustacer-report.html", "HTML page", "html"),
            ReportFormat::Json => ("rustacer-report.json", "JSON file", "json"),
        };
        // The native save dialog is modal by design. Nothing is written anywhere else.
        let Some(mut path) = rfd::FileDialog::new()
            .set_file_name(name)
            .add_filter(label, &[ext])
            .save_file()
        else {
            return;
        };
        if path
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case(ext))
        {
            path.set_extension(ext);
        }
        self.report.open = false;
        self.report.running = true;
        self.report.progress = "Starting...".into();
        self.report.cancel.store(false, Ordering::Relaxed);
        let cancel = self.report.cancel.clone();
        let opts = report::Options {
            format: self.report.format,
            include_identity: self.report.include_identity,
            include_services: self.report.include_services,
        };
        self.spawn(move |bus| {
            let b2 = bus.clone();
            let r = report::export(&path, &opts, &cancel, &mut |p| {
                b2.send(Msg::ReportProgress(p.to_string()))
            });
            bus.send(Msg::ReportDone(match r {
                Ok(()) => Ok(path),
                Err(rustacer_core::CoreError::Cancelled) => {
                    Err("Report export cancelled; nothing was written.".to_string())
                }
                Err(e) => Err(format!("Could not save the report: {e}")),
            }));
        });
    }

    fn report_dialogs(&mut self, ctx: &Context) {
        if self.report.running {
            egui::Window::new("Exporting system report")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Reading: {}", self.report.progress));
                    });
                    if ui.button("Cancel").clicked() {
                        self.report.cancel.store(true, Ordering::Relaxed);
                    }
                });
            return;
        }
        if !self.report.open {
            return;
        }
        let mut go = false;
        let mut close = false;
        egui::Window::new("Export system report")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label("Saves system information, drives and their health, non-Microsoft drivers, startup entries and installed apps to a file you choose.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Format");
                    ui.selectable_value(&mut self.report.format, ReportFormat::Html, "HTML page");
                    ui.selectable_value(&mut self.report.format, ReportFormat::Json, "JSON");
                });
                ui.checkbox(
                    &mut self.report.include_services,
                    "Include third-party services (programs outside the Windows folder)",
                );
                ui.checkbox(
                    &mut self.report.include_identity,
                    "Include the PC name and user name (otherwise they are hidden)",
                );
                if self.report.include_identity {
                    ui.colored_label(
                        Color32::from_rgb(0xf5, 0xa6, 0x23),
                        "The file will contain your PC name, user name and user folder paths. Think before sharing it.",
                    );
                } else {
                    ui.label(
                        egui::RichText::new(
                            "The PC name, the user name and C:\\Users\\<name> paths are replaced with placeholders.",
                        )
                        .weak(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(crate::widgets::primary_btn(ui, "Choose file and export"))
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.report.open = false;
        }
        if go {
            self.start_report();
        }
    }

    /// One immediate read of the connection tables (the 2 s poll takes over afterwards).
    pub fn load_net(&mut self) {
        if self.procs.net.loading {
            return;
        }
        self.procs.net.loading = true;
        self.spawn(|bus| {
            bus.send(Msg::Net(
                netconn::read_connections().map_err(|e| e.to_string()),
            ));
        });
    }

    /// `winget upgrade` listing on a worker thread (read-only, cancellable).
    pub fn check_upgrades(&mut self) {
        if !self.elevated {
            self.notify(
                true,
                "App updates need administrator rights. Use \"Restart as admin\".",
            );
            return;
        }
        let u = &mut self.uninst.updates;
        if u.checking || u.running {
            return;
        }
        u.checking = true;
        u.error = None;
        u.results.clear();
        u.cancel.store(false, Ordering::Relaxed);
        let cancel = u.cancel.clone();
        self.spawn(move |bus| {
            let r = uninstaller::list_upgrades(&cancel);
            bus.send(Msg::Upgrades(r.map(Box::new).map_err(|e| e.to_string())));
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
            Act::WingetUpgrade(list) => {
                if !self.elevated {
                    self.notify(
                        true,
                        "App updates need administrator rights. Use \"Restart as admin\".",
                    );
                    return;
                }
                let list = uninstaller::upgrade_order(list);
                let u = &mut self.uninst.updates;
                u.running = true;
                u.results.clear();
                u.cancel.store(false, Ordering::Relaxed);
                let cancel = u.cancel.clone();
                self.spawn(move |bus| {
                    let mut results = Vec::new();
                    let n = list.len();
                    for (i, pkg) in list.iter().enumerate() {
                        if cancel.load(Ordering::Relaxed) {
                            results.push((pkg.id.clone(), "Skipped (cancelled)".to_string(), None));
                            continue;
                        }
                        bus.send(Msg::UpgradeProgress(format!(
                            "Upgrading {} of {n}: {} - follow winget's own window",
                            i + 1,
                            pkg.name
                        )));
                        info!("user confirmed winget upgrade of {}", pkg.id);
                        let unknown = uninstaller::version_unknown(&pkg.version);
                        let r = uninstaller::winget_upgrade(&pkg.id, unknown);
                        let code = r.as_ref().ok().copied().flatten();
                        let outcome = match r {
                            Ok(Some(0)) => "Finished".to_string(),
                            Ok(Some(code)) => uninstaller::explain_exit_code(code),
                            Ok(None) => "Started (Windows gave no result code)".to_string(),
                            Err(e) => format!("Not run: {e}"),
                        };
                        results.push((pkg.id.clone(), outcome, code));
                    }
                    bus.send(Msg::UpgradeDone(results));
                });
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

    fn soak_step(&mut self, ctx: &Context) {
        let Some(mut s) = self.soak.take() else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(250));
        if !s.started_scan {
            info!("soak test: {} s, scanning {}", s.secs, s.folder);
            self.goto(Page::DiskAnalyzer);
            self.disk.root = s.folder.clone();
            self.scan_disk();
            s.started_scan = true;
        } else if s.t0.elapsed() >= Duration::from_secs(s.secs) {
            info!(
                "soak test done: {} connections, {} processes, {} samples, disk scan {}",
                self.procs.net.list.len(),
                self.procs.list.len(),
                self.history.len(),
                if self.disk.report.is_some() {
                    "finished"
                } else {
                    "not finished"
                }
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        } else if s.last_switch.elapsed() >= Duration::from_secs(60) {
            // Mostly on the Network tab; a short look at the disk tree each minute.
            self.goto(Page::DiskAnalyzer);
            s.last_switch = Instant::now();
        } else if s.last_switch.elapsed() >= Duration::from_secs(5) && self.page != Page::Processes
        {
            self.goto(Page::Processes);
            self.procs.tab = ProcTab::Network;
        }
        self.soak = Some(s);
    }

    fn smoke_step(&mut self, ctx: &Context) {
        let Some((started, step)) = self.smoke else {
            return;
        };
        let Some(t0) = started else {
            let (page, tab) = SMOKE_STEPS[step];
            info!(
                "smoke test: showing {page:?} {tab:?} (loaded so far: {} startup, {} drivers, {} services, {} processes, {} apps, {} history samples, {} drives with health, {} connections, boot log {}, winget {:?})",
                self.startup.entries.len(),
                self.drivers.installed.len(),
                self.services.list.len(),
                self.procs.list.len(),
                self.uninst.list.len(),
                self.history.len(),
                self.health.report.as_ref().map_or(0, |r| r.drives.len()),
                self.procs.net.list.len(),
                match (&self.startup.boot.report, &self.startup.boot.error) {
                    (Some(r), _) => format!("{} boots", r.boots.len()),
                    (None, Some(_)) => "error".to_string(),
                    (None, None) if !self.elevated => "needs admin".to_string(),
                    (None, None) => "loading".to_string(),
                },
                self.uninst.winget_ok
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

/// Sleep up to `ms`, waking early (and returning false) when `stop` is set.
fn nap(stop: &AtomicBool, ms: u64) -> bool {
    let mut left = ms;
    while left > 0 {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let step = left.min(100);
        std::thread::sleep(Duration::from_millis(step));
        left -= step;
    }
    !stop.load(Ordering::Relaxed)
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
    /// Stop the background threads and remove the tray icon before the process ends.
    fn on_exit(&mut self) {
        info!("shutting down");
        self.shutdown.store(true, Ordering::Relaxed);
        self.drivers.cancel.store(true, Ordering::Relaxed);
        self.disk.cancel.store(true, Ordering::Relaxed);
        self.uninst.updates.cancel.store(true, Ordering::Relaxed);
        self.report.cancel.store(true, Ordering::Relaxed);
        self.tray = None; // removes the icon and joins the tray thread
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(t) = &self.tray_test {
            Self::finish_tray_test(t);
        }
    }

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
        self.soak_step(&ctx);
        crate::shots::tick(self, &ctx);
        self.window_keys(&ctx);
        self.handle_close(&ctx);
        self.handle_start_hidden(&ctx);
        self.tray_test_step(&ctx);
        self.confirm_dialog(&ctx);
        self.report_dialogs(&ctx);
        self.driver_dialogs(&ctx);
        // History/gauges move on their own via sampler wakeups; keep a slow tick for notices.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}
