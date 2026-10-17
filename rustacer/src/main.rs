#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod pages;
mod shots;
mod theme;
mod widgets;

use rustacer_core::config::{log_dir, Config};

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let appender = tracing_appender::rolling::daily(dir, "rustacer.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "warn,rustacer=info,rustacer_core=info",
        ))
        .init();
    Some(guard)
}

fn main() -> eframe::Result<()> {
    let _log_guard = init_logging();
    let cfg = Config::load();
    tracing::info!(
        "Rustacer {} starting (elevated: {})",
        env!("CARGO_PKG_VERSION"),
        rustacer_core::admin::is_elevated()
    );

    let capture_run = std::env::args().any(|a| {
        a == "--screenshots"
            || a == "--resize-test"
            || a == "--smoke-test"
            || a == "--tray-test"
            || a == "--soak-test"
    });
    // A copy already running in the tray is brought forward instead of starting a second one
    // (and a second tray icon). Restart-as-admin passes --restarted to skip this.
    let restarted = std::env::args().any(|a| a == rustacer_core::admin::RESTARTED_ARG);
    if !capture_run && !restarted && rustacer_core::tray::signal_running_instance() {
        tracing::info!("another Rustacer is running in the tray; showed it and exiting");
        return Ok(());
    }
    let options = eframe::NativeOptions {
        // Remember size, position and maximized state between launches
        // (stored by eframe under %APPDATA%\Rustacer\data). Test/capture runs never persist.
        persist_window: !capture_run,
        // Test/capture runs keep egui state (zoom, table widths) out of the real profile.
        persistence_path: capture_run.then(|| std::env::temp_dir().join("rustacer-test-state.ron")),
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Rustacer")
            .with_inner_size(if std::env::args().any(|a| a == "--screenshots") {
                [1280.0, 800.0]
            } else {
                [1180.0, 760.0]
            })
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rustacer",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cfg)))),
    )
}
