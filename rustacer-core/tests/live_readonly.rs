//! Live, READ-ONLY smoke tests against the real system. Ignored by default:
//! run with `cargo test -p rustacer-core --test live_readonly -- --ignored --nocapture`.
//! Nothing here installs, deletes or modifies anything.

use rustacer_core::{
    bootperf, drivehealth, drivers, metrics, netconn, services, startup, uninstaller,
};
use std::sync::atomic::AtomicBool;

#[test]
#[ignore]
fn services_list() {
    let v = services::list_services().unwrap();
    println!("{} services", v.len());
    assert!(v.iter().any(|s| s.name.eq_ignore_ascii_case("RpcSs")));
}

#[test]
#[ignore]
fn installed_drivers_and_gpu() {
    let v = drivers::installed::list_installed().unwrap();
    println!("{} drivers", v.len());
    for g in drivers::gpu::detect_gpus(&v) {
        println!("GPU {} {} {}", g.vendor.label(), g.name, g.version);
    }
    assert!(!v.is_empty());
}

#[test]
#[ignore]
fn startup_and_uninstall_lists() {
    let s = startup::list_entries();
    println!("{} startup entries", s.len());
    let u = uninstaller::list_installed();
    println!("{} apps", u.len());
    assert!(!u.is_empty());
}

#[test]
#[ignore]
fn metrics_and_cleaner_scan() {
    let i = metrics::system_info();
    println!("{} | {} | {:?}", i.os, i.cpu_model, i.gpus);
    let cancel = AtomicBool::new(false);
    for t in rustacer_core::cleaner::system_targets() {
        let r = rustacer_core::cleaner::scan_target(&t, &cancel);
        println!(
            "{:?}: {} bytes, {} files, {:?}",
            r.category, r.bytes, r.files, r.unavailable
        );
    }
}

/// Search only: never downloads or installs.
#[test]
#[ignore]
fn windows_update_search_only() {
    let cancel = AtomicBool::new(false);
    let r = drivers::wua::search_driver_updates(&cancel, &mut |p| println!("{}", p.stage));
    match r {
        Ok(mut v) => {
            let installed = drivers::installed::list_installed().unwrap();
            drivers::wua::fill_current(&mut v, &installed);
            for t in drivers::wua::drop_gpu_updates(&mut v, &installed) {
                println!("  [GPU, left out] {t}");
            }
            println!("{} updates offered", v.len());
            for u in v {
                println!(
                    "  {} | hw={} | device={} | current={} -> new={}",
                    u.title, u.hardware_id, u.device_name, u.current_version, u.new_version
                );
            }
        }
        Err(e) => panic!("search failed: {e}"),
    }
}

/// Boot log: needs admin. Not elevated, it must fail with NeedsAdmin instead of guessing.
#[test]
#[ignore]
fn boot_performance_log() {
    match bootperf::read_boot_report() {
        Ok(r) => {
            println!(
                "{} boots, {} slow apps (via {})",
                r.boots.len(),
                r.slow_apps.len(),
                r.source
            );
            if let Some((last, avg, best, worst)) = r.summary() {
                println!("latest {last} ms, avg {avg} ms, best {best} ms, worst {worst} ms");
            }
            for a in r.slow_apps.iter().take(5) {
                println!("  {} slowed boot by {} ms", a.name, a.latest_degradation_ms);
            }
        }
        Err(rustacer_core::CoreError::NeedsAdmin) => {
            assert!(!rustacer_core::admin::is_elevated());
            println!("not elevated: NeedsAdmin, as expected");
        }
        Err(e) => panic!("boot log failed: {e}"),
    }
}

/// Exercises EvtQuery/EvtNext/EvtRender and the XML parser on the Application log, which
/// any user may read.
#[test]
#[ignore]
fn event_log_api_pipeline() {
    let v = bootperf::query_events("Application", "*", 20).unwrap();
    println!("{} events from the Application log", v.len());
    assert!(!v.is_empty());
    assert!(v.iter().all(|e| e.id > 0 && e.time_unix > 0));
}

/// Drive health over WMI. Read-only; counters need admin so they may be absent.
#[test]
#[ignore]
fn drive_health() {
    let r = drivehealth::read_health().unwrap();
    println!("source: {}  counters: {}", r.source, r.counters_available);
    for n in &r.notes {
        println!("note: {n}");
    }
    for d in &r.drives {
        println!(
            "{} | {} {} {} bytes | {} | {:?}",
            d.name,
            d.media,
            d.bus,
            d.size,
            d.health.label(),
            d.counters
        );
        for why in &d.reasons {
            println!("   - {why}");
        }
    }
    assert!(!r.drives.is_empty());
    assert!(r.drives.iter().all(|d| d.size > 0));
}

/// Lists pending winget upgrades (listing only, nothing is upgraded) and checks the GPU
/// exclusion on real output.
#[test]
#[ignore]
fn winget_upgrade_list() {
    if !uninstaller::winget_available() {
        println!("winget not installed; skipping");
        return;
    }
    let cancel = AtomicBool::new(false);
    let l = uninstaller::list_upgrades(&cancel).unwrap();
    println!(
        "{} upgrades offered, {} GPU packages left out",
        l.items.len(),
        l.excluded.len()
    );
    if let Some(n) = &l.note {
        println!("note: {n}");
    }
    for u in &l.items {
        println!(
            "  {} | {} | {} -> {} | {}",
            u.name, u.id, u.version, u.available, u.source
        );
        assert!(uninstaller::gpu_driver_reason(&u.name, &u.id).is_none());
    }
    for (u, why) in &l.excluded {
        println!("  [left out: {why}] {} ({})", u.name, u.id);
    }
}

/// Adds a tray icon for about a second, drives its callback through the simulate hook,
/// then drops it and checks the thread ends. UI only; nothing on the system is changed.
#[test]
#[ignore]
fn tray_icon_lifecycle() {
    use rustacer_core::tray::{Tray, TrayEvent};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};
    let (tx, rx) = channel();
    let tray = Tray::start("Rustacer test", move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    println!("icon shown in the notification area: {}", tray.is_shown());
    tray.set_tooltip("Rustacer - CPU 1% - RAM 2%");
    tray.simulate(TrayEvent::Open);
    tray.simulate(TrayEvent::Quit);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TrayEvent::Open
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        TrayEvent::Quit
    );
    std::thread::sleep(Duration::from_millis(500));
    let t0 = Instant::now();
    drop(tray); // joins the tray thread
    println!("tray stopped in {:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(3));
}

/// Reads the live TCP/UDP tables (IPv4 and IPv6) and resolves owners. Read-only.
#[test]
#[ignore]
fn network_connections() {
    let v = netconn::read_connections().unwrap();
    println!("{} endpoints", v.len());
    let mut m = rustacer_core::processes::ProcessMonitor::new();
    let names: std::collections::HashMap<u32, String> =
        m.refresh().into_iter().map(|p| (p.pid, p.name)).collect();
    for c in v.iter().filter(|c| c.state == "Established").take(5) {
        println!(
            "  {} {} -> {} [{}] pid {} {}",
            c.protocol.label(c.v6),
            c.local_text(),
            c.remote_text(),
            c.state,
            c.pid,
            names.get(&c.pid).map(String::as_str).unwrap_or("?")
        );
    }
    assert!(v.iter().any(|c| c.protocol == netconn::Protocol::Tcp));
    assert!(v.iter().any(|c| c.protocol == netconn::Protocol::Udp));
    assert!(v.iter().any(|c| c.state == "Listen"));
    assert!(v.iter().any(|c| c.v6));
    assert!(v.iter().all(|c| c.local_port > 0 || c.state != "Listen"));
}

/// Collects a real report (read-only), renders it redacted in both formats in memory
/// and checks that this PC's user name and PC name are gone. Nothing is written to disk.
#[test]
#[ignore]
fn system_report_redaction_live() {
    use rustacer_core::report::{self, Format, Options};
    let cancel = AtomicBool::new(false);
    let data = report::collect(true, &cancel, &mut |s| println!("collecting {s}")).unwrap();
    println!(
        "{} drivers, {} startup, {} apps, {} services, {} drives",
        data.drivers.len(),
        data.startup.len(),
        data.apps.len(),
        data.services.as_ref().map_or(0, Vec::len),
        data.health.drives.len()
    );
    let user = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
    let pc = data.pc_name.to_lowercase();
    for format in [Format::Html, Format::Json] {
        let opts = Options {
            format,
            include_identity: false,
            include_services: true,
        };
        let out = report::render(&data, &opts);
        println!("{format:?}: {} bytes", out.len());
        let low = out.to_lowercase();
        if user.len() >= 4 {
            assert!(!low.contains(&user), "{format:?} still has the user name");
        }
        if pc.len() >= 4 {
            assert!(!low.contains(&pc), "{format:?} still has the PC name");
        }
        assert!(out.len() > 1000);
    }
    // The real export path, into a temp folder (never the user's folders).
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.json");
    let opts = Options {
        format: Format::Json,
        ..Options::default()
    };
    report::export(&file, &opts, &cancel, &mut |_| {}).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.trim_start().starts_with('{') && text.trim_end().ends_with('}'));
    if user.len() >= 4 {
        assert!(!text.to_lowercase().contains(&user));
    }
}

/// Turns winget exit codes into winget's own descriptions (runs `winget error`, read-only).
#[test]
#[ignore]
fn winget_error_descriptions() {
    if !uninstaller::winget_available() {
        return;
    }
    let t = uninstaller::explain_exit_code(0x8A15_002B);
    println!("{t}");
    assert!(t.contains("No applicable update"), "{t}");
    let t = uninstaller::explain_exit_code(0x8A15_008E);
    println!("{t}");
    assert!(t.contains("install technology"), "{t}");
}
