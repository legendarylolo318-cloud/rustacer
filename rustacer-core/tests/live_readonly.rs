//! Live, READ-ONLY smoke tests against the real system. Ignored by default:
//! run with `cargo test -p rustacer-core --test live_readonly -- --ignored --nocapture`.
//! Nothing here installs, deletes or modifies anything.

use rustacer_core::{drivers, metrics, services, startup, uninstaller};
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
        Ok(v) => {
            println!("{} updates offered", v.len());
            for u in v {
                println!("  {} [{}]", u.title, u.new_version);
            }
        }
        Err(e) => panic!("search failed: {e}"),
    }
}
