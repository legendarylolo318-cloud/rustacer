//! `--screenshots <dir>`: render a fixed tour of pages and save each frame as a PNG.
//! Used to produce the README images. Only read-only actions are triggered (cleaner scan,
//! Windows Update search), and the PC name is redacted.

use crate::app::{App, DriverTab, Page};
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
}

struct Step {
    name: &'static str,
    page: Page,
    tab: Option<DriverTab>,
    prep: Prep,
    theme: Theme,
}

const fn step(
    name: &'static str,
    page: Page,
    tab: Option<DriverTab>,
    prep: Prep,
    theme: Theme,
) -> Step {
    Step {
        name,
        page,
        tab,
        prep,
        theme,
    }
}

const STEPS: [Step; 9] = [
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

pub struct Shots {
    dir: PathBuf,
    step: usize,
    started: Option<Instant>,
    requested: bool,
}

impl Shots {
    /// Parse `--screenshots <dir>` from the command line.
    pub fn from_args() -> Option<Self> {
        let args: Vec<String> = std::env::args().collect();
        let i = args.iter().position(|a| a == "--screenshots")?;
        let dir = PathBuf::from(args.get(i + 1)?);
        Some(Self {
            dir,
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
    }
}

pub fn tick(app: &mut App, ctx: &Context) {
    let Some(mut s) = app.shots.take() else {
        return;
    };
    ctx.request_repaint();
    let st = &STEPS[s.step];

    let Some(t0) = s.started else {
        info!("screenshots: preparing {}", st.name);
        if let Err(e) = std::fs::create_dir_all(&s.dir) {
            error!("screenshots: cannot create {}: {e}", s.dir.display());
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        app.cfg.theme = st.theme; // session only, never saved
        app.goto(st.page);
        if let Some(t) = st.tab {
            app.drivers.tab = t;
        }
        match st.prep {
            Prep::None => {}
            Prep::ScanCleaner => app.scan_cleaner(),
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
            let path = s.dir.join(format!("{}.png", st.name));
            match save_png(&path, &img) {
                Ok(()) => info!("screenshots: saved {}", path.display()),
                Err(e) => error!("screenshots: saving {} failed: {e}", path.display()),
            }
            if s.step + 1 == STEPS.len() {
                info!("screenshots: done");
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
    let min = if s.step + 1 == STEPS.len() {
        Duration::from_secs(45)
    } else {
        Duration::from_secs(3)
    };
    let ready = waited >= min && prep_done(app, st.prep);
    if ready || waited >= Duration::from_secs(150) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        s.requested = true;
    }
    app.shots = Some(s);
}
