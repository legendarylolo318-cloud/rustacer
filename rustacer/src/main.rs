#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod pages;
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

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Rustacer")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rustacer",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cfg)))),
    )
}
