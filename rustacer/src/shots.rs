//! Capture tours that render pages and save each frame as a PNG.
//!
//! * `--screenshots <dir>`: the README images. Only read-only actions are triggered
//!   (cleaner scan, Windows Update search), and the PC name is redacted.
//! * `--resize-test <dir>`: drives the window through small, large, maximized and
//!   fullscreen states and captures pages in each, to catch layout bugs.

use crate::app::{App, DriverTab, Page, ProcTab, StartupTab, Sub};
use eframe::egui::{self, Context};
use rustacer_core::config::Theme;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info};

#[derive(Clone, Copy)]
enum Prep {
    None,
    ScanCleaner,
    ScanDrivers,
    /// Scan the folder holding this executable, or System32 for the README images (read-only).
    ScanDisk,
    /// Show only Windows service host sockets on the Network tab (no personal apps).
    NetListen,
}

#[derive(Clone, Copy, Debug)]
enum Win {
    Size(f32, f32),
    Maximized,
    Fullscreen,
}

struct Step {
    name: &'static str,
    page: Page,
    tab: Option<DriverTab>,
    /// Tab inside the page (other than the driver tabs).
    sub: Sub,
    prep: Prep,
    theme: Theme,
    win: Option<Win>,
    /// UI scale for this step (session only).
    scale: f32,
    /// Minimum time on this step before capturing.
    dwell: Duration,
}

fn step(name: &'static str, page: Page, tab: Option<DriverTab>, prep: Prep, theme: Theme) -> Step {
    Step {
        name,
        page,
        tab,
        sub: Sub::None,
        prep,
        theme,
        win: None,
        scale: 1.0,
        dwell: Duration::from_secs(3),
    }
}

fn readme_tour() -> Vec<Step> {
    let mut v = vec![
        step("dashboard", Page::Dashboard, None, Prep::None, Theme::Dark),
        step(
            "cleaner",
            Page::Cleaner,
            None,
            Prep::ScanCleaner,
            Theme::Dark,
        ),
        step(
            "drivers-installed",
            Page::Drivers,
            Some(DriverTab::Installed),
            Prep::None,
            Theme::Dark,
        ),
        step(
            "drivers-updates",
            Page::Drivers,
            Some(DriverTab::Updates),
            Prep::ScanDrivers,
            Theme::Dark,
        ),
        step(
            "drivers-gpu",
            Page::Drivers,
            Some(DriverTab::Gpu),
            Prep::None,
            Theme::Dark,
        ),
        step(
            "disk",
            Page::DiskAnalyzer,
            None,
            Prep::ScanDisk,
            Theme::Dark,
        ),
        step("health", Page::Health, None, Prep::None, Theme::Dark),
        {
            // svchost only: no personal apps or browsing in the image.
            let mut s = step(
                "network",
                Page::Processes,
                None,
                Prep::NetListen,
                Theme::Dark,
            );
            s.sub = Sub::Procs(ProcTab::Network);
            s
        },
        step("services", Page::Services, None, Prep::None, Theme::Dark),
        step("settings", Page::Settings, None, Prep::None, Theme::Dark),
        step(
            "settings-nord",
            Page::Settings,
            None,
            Prep::None,
            Theme::Nord,
        ),
        // Last, so the graphs have collected a minute or so of history.
        step(
            "resources",
            Page::Resources,
            None,
            Prep::None,
            Theme::Dracula,
        ),
    ];
    if let Some(last) = v.last_mut() {
        last.dwell = Duration::from_secs(45);
    }
    v
}

fn resize_tour() -> Vec<Step> {
    let states = [
        ("min", Win::Size(900.0, 560.0)),
        ("large", Win::Size(1600.0, 1000.0)),
        ("maximized", Win::Maximized),
        ("fullscreen", Win::Fullscreen),
        ("restored", Win::Size(1180.0, 760.0)),
    ];
    let pages = [
        ("dashboard", Page::Dashboard, None),
        ("disk", Page::DiskAnalyzer, None),
        ("health", Page::Health, None),
        ("drivers", Page::Drivers, Some(DriverTab::Installed)),
        ("services", Page::Services, None),
        ("resources", Page::Resources, None),
    ];
    let mut v = Vec::new();
    for (sname, win) in states {
        for (i, (pname, page, tab)) in pages.iter().enumerate() {
            let name: &'static str = Box::leak(format!("{sname}-{pname}").into_boxed_str());
            let prep = if *page == Page::DiskAnalyzer {
                Prep::ScanDisk
            } else {
                Prep::None
            };
            let mut s = step(name, *page, *tab, prep, Theme::Dark);
            // Change the window once per state, on its first page.
            if i == 0 {
                s.win = Some(win);
            }
            s.dwell = Duration::from_millis(if i == 0 { 2500 } else { 1500 });
            v.push(s);
        }
    }
    // Accessibility: high contrast, and 150% scale at the minimum window size.
    let mut hc = step(
        "a11y-highcontrast-settings",
        Page::Settings,
        None,
        Prep::None,
        Theme::HighContrast,
    );
    hc.dwell = Duration::from_millis(1500);
    v.push(hc);
    let mut hcd = step(
        "a11y-highcontrast-drivers",
        Page::Drivers,
        Some(DriverTab::Installed),
        Prep::None,
        Theme::HighContrast,
    );
    hcd.dwell = Duration::from_millis(1500);
    v.push(hcd);
    let mut big = step(
        "a11y-scale150-min-services",
        Page::Services,
        None,
        Prep::None,
        Theme::Dark,
    );
    big.win = Some(Win::Size(900.0, 560.0));
    big.scale = 1.5;
    big.dwell = Duration::from_millis(2500);
    v.push(big);
    // The newer pages and tabs at 150% in the smallest window, and in high contrast.
    let newer: [(&str, Page, Sub, Prep); 7] = [
        ("disk", Page::DiskAnalyzer, Sub::None, Prep::ScanDisk),
        ("dashboard", Page::Dashboard, Sub::None, Prep::None),
        ("health", Page::Health, Sub::None, Prep::None),
        (
            "boot",
            Page::Startup,
            Sub::Startup(StartupTab::Boot),
            Prep::None,
        ),
        ("updates", Page::AppUpdates, Sub::None, Prep::None),
        (
            "network",
            Page::Processes,
            Sub::Procs(ProcTab::Network),
            Prep::None,
        ),
        ("settings", Page::Settings, Sub::None, Prep::None),
    ];
    for (i, (name, page, sub, prep)) in newer.iter().enumerate() {
        let n: &'static str = Box::leak(format!("a11y-scale150-min-{name}").into_boxed_str());
        let mut st = step(n, *page, None, *prep, Theme::Dark);
        st.sub = *sub;
        st.scale = 1.5;
        if i == 0 {
            st.win = Some(Win::Size(900.0, 560.0));
        }
        st.dwell = Duration::from_millis(2000);
        v.push(st);
    }
    for (i, (name, page, sub, prep)) in newer.iter().enumerate() {
        let n: &'static str = Box::leak(format!("a11y-highcontrast-{name}").into_boxed_str());
        let mut st = step(n, *page, None, *prep, Theme::HighContrast);
        st.sub = *sub;
        if i == 0 {
            st.win = Some(Win::Size(1180.0, 760.0));
        }
        st.dwell = Duration::from_millis(1500);
        v.push(st);
    }
    // Tabs inside pages.
    let tabs: Vec<(&'static str, Page, Sub)> = vec![
        (
            "tab-startup-boot",
            Page::Startup,
            Sub::Startup(StartupTab::Boot),
        ),
        (
            "tab-processes-network",
            Page::Processes,
            Sub::Procs(ProcTab::Network),
        ),
    ];
    for (name, page, sub) in tabs {
        let mut st = step(name, page, None, Prep::None, Theme::Dark);
        st.sub = sub;
        st.dwell = Duration::from_millis(1500);
        v.push(st);
    }
    v
}

pub struct Shots {
    dir: PathBuf,
    steps: Vec<Step>,
    step: usize,
    started: Option<Instant>,
    requested: bool,
}

impl Shots {
    /// Parse `--screenshots <dir>` or `--resize-test <dir>` from the command line.
    pub fn from_args() -> Option<Self> {
        let args: Vec<String> = std::env::args().collect();
        let (i, steps) = if let Some(i) = args.iter().position(|a| a == "--screenshots") {
            (i, readme_tour())
        } else {
            let i = args.iter().position(|a| a == "--resize-test")?;
            (i, resize_tour())
        };
        let dir = PathBuf::from(args.get(i + 1)?);
        Some(Self {
            dir,
            steps,
            step: 0,
            started: None,
            requested: false,
        })
    }
}

fn save_png(path: &Path, img: &egui::ColorImage) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        img.width() as u32,
        img.height() as u32,
    );
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(img.as_raw()).map_err(|e| e.to_string())
}

fn prep_done(app: &App, prep: Prep) -> bool {
    match prep {
        Prep::None => true,
        Prep::ScanCleaner => !app.cleaner.scanning,
        Prep::ScanDrivers => app.drivers.scanned_once && !app.drivers.scanning,
        Prep::ScanDisk => app.disk.report.is_some() && !app.disk.scanning,
        Prep::NetListen => app.procs.net.loaded,
    }
}

fn apply_window(ctx: &Context, w: Win) {
    use egui::ViewportCommand as V;
    match w {
        Win::Size(x, y) => {
            ctx.send_viewport_cmd(V::Fullscreen(false));
            ctx.send_viewport_cmd(V::Maximized(false));
            ctx.send_viewport_cmd(V::InnerSize(egui::vec2(x, y)));
        }
        Win::Maximized => {
            ctx.send_viewport_cmd(V::Fullscreen(false));
            ctx.send_viewport_cmd(V::Maximized(true));
        }
        Win::Fullscreen => ctx.send_viewport_cmd(V::Fullscreen(true)),
    }
}

pub fn tick(app: &mut App, ctx: &Context) {
    let Some(mut s) = app.shots.take() else {
        return;
    };
    ctx.request_repaint();
    let st = &s.steps[s.step];

    let Some(t0) = s.started else {
        info!("capture: preparing {}", st.name);
        if let Err(e) = std::fs::create_dir_all(&s.dir) {
            error!("capture: cannot create {}: {e}", s.dir.display());
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if let Some(w) = st.win {
            info!("capture: window -> {w:?}");
            apply_window(ctx, w);
        }
        app.cfg.theme = st.theme; // session only, never saved
        app.notice = None; // a notice from an earlier step would clutter the image
        app.cfg.ui_scale = st.scale;
        app.goto(st.page);
        if let Some(t) = st.tab {
            app.drivers.tab = t;
        }
        app.show_sub(st.sub);
        match st.prep {
            Prep::None => {}
            Prep::ScanCleaner => app.scan_cleaner(),
            Prep::ScanDisk => {
                // README images scan a system folder, so no user name or personal file shows.
                let dir = if app.redact {
                    std::env::var_os("SystemRoot")
                        .map(|w| std::path::PathBuf::from(w).join("System32"))
                } else {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                };
                if let Some(dir) = dir {
                    app.disk.root = dir.to_string_lossy().into_owned();
                    app.scan_disk();
                }
            }
            Prep::NetListen => app.procs.net.filter = "svchost".into(),
            Prep::ScanDrivers => {
                app.drivers.show_all_updates = true;
                app.scan_driver_updates();
            }
        }
        s.started = Some(Instant::now());
        app.shots = Some(s);
        return;
    };

    if s.requested {
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let (screen, inner, ppp, maxed, full) = ctx.input(|i| {
                let vp = i.viewport();
                (
                    i.content_rect(),
                    vp.inner_rect,
                    i.pixels_per_point,
                    vp.maximized,
                    vp.fullscreen,
                )
            });
            info!(
                "capture: {} image={}x{} content={:?} inner={:?} ppp={ppp} maximized={:?} fullscreen={:?}",
                st.name,
                img.width(),
                img.height(),
                screen,
                inner,
                maxed,
                full
            );
            let path = s.dir.join(format!("{}.png", st.name));
            match save_png(&path, &img) {
                Ok(()) => info!("capture: saved {}", path.display()),
                Err(e) => error!("capture: saving {} failed: {e}", path.display()),
            }
            if s.step + 1 == s.steps.len() {
                info!("capture: done");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            s.step += 1;
            s.started = None;
            s.requested = false;
        }
        app.shots = Some(s);
        return;
    }

    let waited = t0.elapsed();
    let ready = waited >= st.dwell && prep_done(app, st.prep);
    if ready || waited >= Duration::from_secs(150) {
        app.notice = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        s.requested = true;
    }
    app.shots = Some(s);
}
