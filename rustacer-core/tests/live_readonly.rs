//! Live, READ-ONLY smoke tests against the real system. Ignored by default:
//! run with `cargo test -p rustacer-core --test live_readonly -- --ignored --nocapture`.
//! Nothing here installs, deletes or modifies anything.

use rustacer_core::{bootperf, drivehealth, drivers, metrics, services, startup, uninstaller};
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
