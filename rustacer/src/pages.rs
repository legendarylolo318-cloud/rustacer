use crate::app::*;
use crate::widgets::{admin_banner, danger_btn, gauge, heading, primary_btn};
use eframe::egui::{self, Color32, Context, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Line, Plot, PlotPoints};
use rustacer_core::cleaner::Category;
use rustacer_core::config::Theme;
use rustacer_core::drivers::gpu::{detect_gpus, GpuVendor};
use rustacer_core::drivers::installed::matches_filter;
use rustacer_core::drivers::wua;
use rustacer_core::metrics::format_uptime;
use rustacer_core::services::{self, Action as SvcAction, ServiceState, StartType};
use rustacer_core::startup::Source;
use rustacer_core::strings::{tr, Lang};
use rustacer_core::util::{format_bytes, open_path, reveal_in_explorer};

const ROW_H: f32 = 26.0;

pub fn sidebar(app: &mut App, ui: &mut Ui, ctx: &Context) {
    egui::Panel::left("nav")
        .exact_size(196.0)
        .resizable(false)
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Rustacer");
            ui.label(RichText::new("System optimizer").weak());
            ui.add_space(10.0);
            for p in Page::ALL {
                let sel = app.page == p;
                if ui
                    .add_sized(
                        [ui.available_width(), 38.0],
                        egui::Button::selectable(sel, tr(Lang::English, p.key())),
                    )
                    .clicked()
                {
                    app.goto(p);
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(8.0);
                if app.elevated {
                    ui.colored_label(
                        Color32::from_rgb(0x3d, 0xb8, 0x6b),
                        "Running as administrator",
                    );
                } else {
                    ui.label(RichText::new("Standard user mode").weak());
                    if ui
                        .add(primary_btn(ui, tr(Lang::English, "admin.restart")))
                        .clicked()
                    {
                        app.restart_as_admin(ctx);
                    }
                }
            });
        });
}

pub fn show(app: &mut App, ui: &mut Ui, ctx: &Context) {
    let elevated = app.elevated;
    let needs_admin = matches!(
        app.page,
        Page::Cleaner | Page::Drivers | Page::Services | Page::Startup
    );
    if admin_banner(ui, elevated, needs_admin) {
        app.restart_as_admin(ctx);
    }
    match app.page {
        Page::Dashboard => dashboard(app, ui),
        Page::Startup => startup(app, ui),
        Page::Cleaner => cleaner(app, ui),
        Page::Drivers => drivers(app, ui),
        Page::Services => services_page(app, ui),
        Page::Processes => processes(app, ui),
        Page::Uninstaller => uninstaller(app, ui),
        Page::Resources => resources(app, ui),
        Page::Settings => settings(app, ui),
    }
}

fn search_box(ui: &mut Ui, text: &mut String) {
    ui.horizontal(|ui| {
        ui.label(tr(Lang::English, "common.search"));
        ui.add(egui::TextEdit::singleline(text).desired_width(240.0));
    });
}

// ---------------- dashboard ----------------

fn dashboard(app: &mut App, ui: &mut Ui) {
    heading(ui, "Dashboard");
    egui::ScrollArea::vertical().show(ui, |ui| {
        let s = &app.snap;
        ui.horizontal_wrapped(|ui| {
            gauge(
                ui,
                "CPU",
                s.cpu_total,
                &format!("{} cores", s.cpu_cores.len()),
            );
            gauge(
                ui,
                "Memory",
                s.ram_percent(),
                &format!(
                    "{} / {}",
                    format_bytes(s.ram_used),
                    format_bytes(s.ram_total)
                ),
            );
            for d in &s.disks {
                gauge(
                    ui,
                    &format!("Disk {}", d.mount.trim_end_matches('\\')),
                    d.used_percent(),
                    &format!("{} free", format_bytes(d.available)),
                );
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Down  {}/s", format_bytes(s.net_down_bps as u64))).strong(),
            );
            ui.add_space(16.0);
            ui.label(
                RichText::new(format!("Up  {}/s", format_bytes(s.net_up_bps as u64))).strong(),
            );
            ui.add_space(16.0);
            ui.label(format!("Uptime  {}", format_uptime(s.uptime_secs)));
        });
        ui.separator();
        ui.heading("System information");
        match &app.info {
            None => {
                ui.spinner();
            }
            Some(i) => {
                egui::Grid::new("sysinfo")
                    .num_columns(2)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        let boot = format_boot(i.boot_time_unix);
                        let rows: Vec<(&str, String)> = vec![
                            (
                                "Hostname",
                                if app.redact {
                                    "DEMO-PC".to_string()
                                } else {
                                    i.hostname.clone()
                                },
                            ),
                            ("OS", format!("{} (build {})", i.os, i.os_build)),
                            (
                                "CPU",
                                format!(
                                    "{} ({} cores / {} threads)",
                                    i.cpu_model, i.physical_cores, i.logical_cores
                                ),
                            ),
                            (
                                "GPU",
                                if i.gpus.is_empty() {
                                    "Unknown".into()
                                } else {
                                    i.gpus.join(", ")
                                },
                            ),
                            ("Total RAM", format_bytes(i.total_ram)),
                            ("Boot time", boot),
                        ];
                        for (k, v) in rows {
                            ui.label(RichText::new(k).strong());
                            ui.label(v);
                            ui.end_row();
                        }
                    });
            }
        }
    });
}

fn format_boot(unix: u64) -> String {
    let secs = unix % 86_400;
    let d = rustacer_core::drivers::version::Date::from_ole((unix / 86_400) as f64 + 25_569.0);
    match d {
        Some(d) => format!("{} {:02}:{:02} UTC", d, secs / 3600, secs % 3600 / 60),
        None => "Unknown".into(),
    }
}

// ---------------- startup ----------------

fn startup(app: &mut App, ui: &mut Ui) {
    heading(ui, "Startup Apps");
    let mut act: Option<Act> = None;
    ui.horizontal(|ui| {
        search_box(ui, &mut app.startup.filter);
        if ui.button(tr(Lang::English, "common.refresh")).clicked() {
            app.load_page(Page::Startup, true);
        }
        if app.startup.loading {
            ui.spinner();
        }
    });
    ui.collapsing("Add startup entry", |ui| {
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut app.startup.new_name);
        });
        ui.horizontal(|ui| {
            ui.label("Program");
            ui.add(egui::TextEdit::singleline(&mut app.startup.new_exe).desired_width(360.0));
            if ui.button("Browse...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Programs", &["exe"])
                    .pick_file()
                {
                    app.startup.new_exe = p.to_string_lossy().into_owned();
                    if app.startup.new_name.is_empty() {
                        app.startup.new_name = p
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Arguments");
            ui.text_edit_singleline(&mut app.startup.new_args);
        });
        if ui.add(primary_btn(ui, "Add (current user)")).clicked() {
            act = Some(Act::StartupAdd {
                name: app.startup.new_name.clone(),
                exe: app.startup.new_exe.clone(),
                args: app.startup.new_args.clone(),
            });
        }
    });
    ui.add_space(4.0);
    let f = app.startup.filter.to_lowercase();
    let rows: Vec<usize> = app
        .startup
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            f.is_empty()
                || e.name.to_lowercase().contains(&f)
                || e.command.to_lowercase().contains(&f)
        })
        .map(|(i, _)| i)
        .collect();
    let mut toggle: Option<(usize, bool)> = None;
    let mut delete: Option<usize> = None;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .column(Column::exact(70.0))
        .column(Column::initial(200.0).clip(true))
        .column(Column::initial(170.0).clip(true))
        .column(Column::remainder().clip(true))
        .column(Column::exact(60.0))
        .header(ROW_H, |mut h| {
            for t in ["Enabled", "Name", "Source", "Command", ""] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let i = rows[row.index()];
                let e = &app.startup.entries[i];
                row.col(|ui| {
                    let mut on = e.enabled;
                    if ui.checkbox(&mut on, "").changed() {
                        toggle = Some((i, on));
                    }
                });
                row.col(|ui| {
                    ui.label(&e.name);
                });
                row.col(|ui| {
                    ui.label(e.source.label());
                });
                row.col(|ui| {
                    ui.label(&e.command).on_hover_text(&e.command);
                });
                row.col(|ui| {
                    if ui.small_button("Delete").clicked() {
                        delete = Some(i);
                    }
                });
            });
        });
    if let Some((i, on)) = toggle {
        let e = app.startup.entries[i].clone();
        let admin_note = (e.source.needs_admin() && !app.elevated)
            .then(|| "This entry needs administrator rights.".to_string());
        app.ask(
            if on {
                "Enable startup entry"
            } else {
                "Disable startup entry"
            },
            format!(
                "{} '{}'?\nThis is reversible.",
                if on { "Enable" } else { "Disable" },
                e.name
            ),
            admin_note,
            Act::StartupSet(e, on),
        );
    }
    if let Some(i) = delete {
        let e = app.startup.entries[i].clone();
        let warn = if matches!(e.source, Source::Task) {
            Some("Deleting a scheduled task cannot be undone.".into())
        } else {
            Some("Deleting cannot be undone. Disable it instead if unsure.".into())
        };
        app.ask(
            "Delete startup entry",
            format!("Permanently delete '{}'?", e.name),
            warn,
            Act::StartupDelete(e),
        );
    }
    if let Some(a) = act {
        app.ask(
            "Add startup entry",
            "Add this program to your Windows startup?".into(),
            None,
            a,
        );
    }
}

// ---------------- cleaner ----------------

fn cleaner(app: &mut App, ui: &mut Ui) {
    heading(ui, "System Cleaner");
    ui.label("Only whitelisted cache and temp locations are scanned. Cookies, history and browser profiles are never touched.");
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let busy = app.cleaner.scanning || app.cleaner.cleaning;
        if ui.add_enabled(!busy, primary_btn(ui, "Scan")).clicked() {
            app.scan_cleaner();
        }
        if busy {
            ui.spinner();
        }
    });
    ui.add_space(6.0);
    let mut clean: Option<Vec<Category>> = None;
    if !app.cleaner.results.is_empty() {
        egui::Grid::new("clean")
            .num_columns(4)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                for (r, checked) in app.cleaner.results.iter_mut() {
                    ui.add_enabled(
                        r.unavailable.is_none(),
                        egui::Checkbox::new(checked, r.category.label()),
                    );
                    ui.label(format_bytes(r.bytes));
                    ui.label(if r.files == 1 {
                        "1 file".to_string()
                    } else {
                        format!("{} files", r.files)
                    });
                    if let Some(u) = &r.unavailable {
                        ui.colored_label(Color32::from_rgb(0xf5, 0xa6, 0x23), u);
                    } else {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
        let total: u64 = app
            .cleaner
            .results
            .iter()
            .filter(|(_, c)| *c)
            .map(|(r, _)| r.bytes)
            .sum();
        ui.add_space(8.0);
        if ui
            .add_enabled(
                total > 0 && !app.cleaner.cleaning,
                primary_btn(ui, format!("Clean selected ({})", format_bytes(total))),
            )
            .clicked()
        {
            clean = Some(
                app.cleaner
                    .results
                    .iter()
                    .filter(|(_, c)| *c)
                    .map(|(r, _)| r.category)
                    .collect(),
            );
        }
    }
    if let Some(cats) = clean {
        let list: Vec<&str> = cats.iter().map(|c| c.label()).collect();
        app.ask(
            "Delete selected files",
            format!(
                "Delete the contents of:\n- {}\n\nFiles in use are skipped and reported.",
                list.join("\n- ")
            ),
            Some(
                "Deleted files cannot be recovered (the Recycle Bin is emptied permanently)."
                    .into(),
            ),
            Act::Clean(cats),
        );
    }
    if let Some(r) = &app.cleaner.report {
        ui.add_space(10.0);
        ui.separator();
        ui.label(
            RichText::new(format!(
                "Freed {} ({} files deleted).",
                format_bytes(r.freed_bytes),
                r.files_deleted
            ))
            .strong(),
        );
        if !r.skipped.is_empty() {
            egui::CollapsingHeader::new(format!("{} item(s) skipped", r.skipped.len())).show(
                ui,
                |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for s in r.skipped.iter().take(300) {
                                ui.label(format!("{} - {}", s.path.display(), s.reason));
                            }
                        });
                },
            );
        }
    }
}

// ---------------- drivers ----------------

fn drivers(app: &mut App, ui: &mut Ui) {
    heading(ui, "Driver Updater");
    ui.label(RichText::new("Updates come from Windows Update only. A System Restore point is created before any install.").weak());
    ui.horizontal(|ui| {
        for (t, label) in [
            (DriverTab::Installed, "Installed drivers"),
            (DriverTab::Updates, "Updates"),
            (DriverTab::Backup, "Backup"),
            (DriverTab::Gpu, "GPU"),
            (DriverTab::RestorePoints, "Restore points"),
        ] {
            if ui.selectable_label(app.drivers.tab == t, label).clicked() {
                app.drivers.tab = t;
            }
        }
    });
    ui.separator();
    match app.drivers.tab {
        DriverTab::Installed => drivers_installed(app, ui),
        DriverTab::Updates => drivers_updates(app, ui),
        DriverTab::Backup => drivers_backup(app, ui),
        DriverTab::Gpu => drivers_gpu(app, ui),
        DriverTab::RestorePoints => restore_points(app, ui),
    }
}

fn drivers_installed(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        search_box(ui, &mut app.drivers.filter);
        if ui
            .checkbox(
                &mut app.cfg.hide_inbox_drivers,
                "Hide Microsoft inbox drivers",
            )
            .changed()
        {
            app.save_cfg();
        }
        ui.checkbox(&mut app.drivers.only_old, "Only old");
        if ui.button(tr(Lang::English, "common.refresh")).clicked() {
            app.load_page(Page::Drivers, true);
        }
        if app.drivers.loading {
            ui.spinner();
        }
    });
    let years = app.cfg.old_driver_years;
    let rows: Vec<usize> = app
        .drivers
        .installed
        .iter()
        .enumerate()
        .filter(|(_, d)| matches_filter(d, &app.drivers.filter))
        .filter(|(_, d)| !(app.cfg.hide_inbox_drivers && d.is_inbox()))
        .filter(|(_, d)| !app.drivers.only_old || d.is_old(years))
        .map(|(i, _)| i)
        .collect();
    ui.label(
        RichText::new(format!(
            "{} drivers. \"Old\" = older than {years} years (information only).",
            rows.len()
        ))
        .weak(),
    );
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .column(Column::initial(230.0).clip(true))
        .column(Column::initial(90.0).clip(true))
        .column(Column::initial(150.0).clip(true))
        .column(Column::initial(110.0).clip(true))
        .column(Column::initial(90.0))
        .column(Column::initial(90.0).clip(true))
        .column(Column::initial(60.0))
        .column(Column::remainder())
        .header(ROW_H, |mut h| {
            for t in [
                "Device", "Class", "Provider", "Version", "Date", "INF", "Signed", "",
            ] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let d = &app.drivers.installed[rows[row.index()]];
                row.col(|ui| {
                    ui.label(&d.device_name);
                });
                row.col(|ui| {
                    ui.label(&d.device_class);
                });
                row.col(|ui| {
                    ui.label(if d.provider.is_empty() {
                        &d.manufacturer
                    } else {
                        &d.provider
                    });
                });
                row.col(|ui| {
                    ui.label(&d.version);
                });
                row.col(|ui| {
                    ui.label(d.date.map(|x| x.to_string()).unwrap_or_default());
                });
                row.col(|ui| {
                    ui.label(&d.inf_name);
                });
                row.col(|ui| {
                    ui.label(match d.signed {
                        Some(true) => "Yes",
                        Some(false) => "No",
                        None => "?",
                    });
                });
                row.col(|ui| {
                    if d.is_old(years) {
                        ui.colored_label(Color32::from_rgb(0xf5, 0xa6, 0x23), "old");
                    }
                });
            });
        });
}

fn drivers_updates(app: &mut App, ui: &mut Ui) {
    let mut install: Option<Vec<wua::DriverUpdate>> = None;
    ui.horizontal(|ui| {
        let busy = app.drivers.scanning || app.drivers.installing;
        if ui
            .add_enabled(!busy, primary_btn(ui, "Scan for driver updates"))
            .clicked()
        {
            app.scan_driver_updates();
        }
        if app.drivers.scanning && ui.button(tr(Lang::English, "common.cancel")).clicked() {
            app.drivers
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if app.drivers.scanning || app.drivers.installing {
            ui.spinner();
        }
    });
    if let Some(p) = app
        .drivers
        .scan_progress
        .as_ref()
        .filter(|_| app.drivers.scanning)
    {
        ui.label(&p.stage);
    }
    if app.drivers.installing {
        if let Some(p) = &app.drivers.install_progress {
            ui.label(&p.stage);
            if let Some(pc) = p.percent {
                ui.add(egui::ProgressBar::new(pc as f32 / 100.0).show_percentage());
            }
            if p.stage.starts_with("Downloading") && ui.button("Cancel download").clicked() {
                app.drivers
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        } else {
            ui.label("Creating System Restore point...");
        }
    }
    if let Some(s) = &app.drivers.summary {
        ui.separator();
        ui.label(RichText::new("Installation results").strong());
        for o in &s.outcomes {
            let c = if o.success {
                Color32::from_rgb(0x3d, 0xb8, 0x6b)
            } else {
                Color32::from_rgb(0xe5, 0x48, 0x4d)
            };
            ui.colored_label(c, format!("{} - {}", o.title, o.result));
        }
        if s.reboot_required {
            ui.colored_label(
                Color32::from_rgb(0xf5, 0xa6, 0x23),
                "A reboot is required to finish installation.",
            );
            if ui.add(primary_btn(ui, "Reboot now")).clicked() {
                app.confirm = Some(Confirm {
                    title: "Restart Windows".into(),
                    body: "Restart the computer in 10 seconds? Save your work first.".into(),
                    warning: None,
                    act: Act::RebootNow,
                });
            }
        }
        ui.separator();
    }
    if !app.drivers.gpu_skipped.is_empty() && !app.drivers.scanning {
        let n = app.drivers.gpu_skipped.len();
        let list = app.drivers.gpu_skipped.join("\n");
        let mut open_gpu_tab = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{n} GPU driver offer(s) left out. Rustacer never installs GPU drivers."
                ))
                .weak(),
            )
            .on_hover_text(list);
            if ui
                .link("Open the GPU tab for official vendor pages")
                .clicked()
            {
                open_gpu_tab = true;
            }
        });
        if open_gpu_tab {
            app.drivers.tab = DriverTab::Gpu;
        }
    }
    if app.drivers.updates.is_empty() {
        if app.drivers.scanned_once && !app.drivers.scanning {
            ui.label("No non-GPU driver updates are offered by Windows Update.");
        }
        return;
    }
    use wua::UpdateStatus;
    let visible: Vec<usize> = app
        .drivers
        .updates
        .iter()
        .enumerate()
        .filter(|(_, (u, _))| app.drivers.show_all_updates || wua::status(u) == UpdateStatus::Newer)
        .map(|(i, _)| i)
        .collect();
    let hidden = app.drivers.updates.len() - visible.len();
    let n_sel = visible
        .iter()
        .filter(|&&i| app.drivers.updates[i].1)
        .count();
    ui.horizontal(|ui| {
        if ui.button("Select all").clicked() {
            for &i in &visible {
                app.drivers.updates[i].1 = true;
            }
        }
        if ui.button("Select none").clicked() {
            app.drivers.updates.iter_mut().for_each(|(_, c)| *c = false);
        }
        if ui
            .add_enabled(
                n_sel > 0 && !app.drivers.installing,
                primary_btn(ui, format!("Install selected ({n_sel})")),
            )
            .clicked()
        {
            install = Some(
                visible
                    .iter()
                    .filter(|&&i| app.drivers.updates[i].1)
                    .map(|&i| app.drivers.updates[i].0.clone())
                    .collect(),
            );
        }
        if ui
            .checkbox(
                &mut app.drivers.show_all_updates,
                format!("Show offers that are not newer ({hidden} hidden)"),
            )
            .changed()
            && !app.drivers.show_all_updates
        {
            // Never keep a hidden row selected.
            for (u, c) in app.drivers.updates.iter_mut() {
                if wua::status(u) != UpdateStatus::Newer {
                    *c = false;
                }
            }
        }
    });
    if visible.is_empty() {
        ui.label(
            "Every driver Windows Update offers is the same as or older than what is installed.",
        );
        return;
    }
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .column(Column::exact(30.0))
        .column(Column::initial(190.0).clip(true))
        .column(Column::initial(120.0))
        .column(Column::initial(110.0).clip(true))
        .column(Column::initial(110.0).clip(true))
        .column(Column::initial(90.0))
        .column(Column::initial(80.0))
        .column(Column::remainder().clip(true))
        .header(ROW_H, |mut h| {
            for t in [
                "", "Device", "Status", "Current", "New", "Date", "Size", "Title",
            ] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, visible.len(), |mut row| {
                let (u, c) = &mut app.drivers.updates[visible[row.index()]];
                let st = wua::status(u);
                row.col(|ui| {
                    ui.checkbox(c, "");
                });
                row.col(|ui| {
                    ui.label(if u.device_name.is_empty() {
                        "(unmatched)"
                    } else {
                        &u.device_name
                    });
                });
                row.col(|ui| {
                    let color = match st {
                        UpdateStatus::Newer => Color32::from_rgb(0x3d, 0xb8, 0x6b),
                        UpdateStatus::Older => Color32::from_rgb(0xe5, 0x48, 0x4d),
                        _ => Color32::from_rgb(0xf5, 0xa6, 0x23),
                    };
                    ui.colored_label(color, st.label());
                });
                row.col(|ui| {
                    ui.label(if u.current_version.is_empty() {
                        "-"
                    } else {
                        &u.current_version
                    });
                });
                row.col(|ui| {
                    ui.label(if u.new_version.is_empty() {
                        "-"
                    } else {
                        &u.new_version
                    });
                });
                row.col(|ui| {
                    ui.label(u.new_date.map(|d| d.to_string()).unwrap_or_default());
                });
                row.col(|ui| {
                    ui.label(format_bytes(u.size_bytes));
                });
                row.col(|ui| {
                    ui.label(&u.title).on_hover_text(&u.title);
                });
            });
        });
    if let Some(list) = install {
        if !app.elevated {
            app.notify(
                true,
                "Installing drivers requires administrator rights. Use \"Restart as admin\" first.",
            );
            return;
        }
        let titles: Vec<String> = list
            .iter()
            .map(|u| format!("- {} [{}]", u.title, wua::status(u).label()))
            .collect();
        let risky: Vec<&str> = list
            .iter()
            .filter(|u| wua::status(u) != UpdateStatus::Newer)
            .map(|u| u.title.as_str())
            .collect();
        let warn = (!risky.is_empty()).then(|| {
            format!(
                "{} selected update(s) are NOT newer than the installed driver (same version, downgrade or unknown):\n- {}",
                risky.len(),
                risky.join("\n- ")
            )
        });
        app.ask(
            "Install driver updates",
            format!(
                "A System Restore point will be created first, then these updates are downloaded and installed from Windows Update:\n{}\n\nOnly these selected updates are installed.",
                titles.join("\n")
            ),
            warn,
            Act::InstallDrivers(list),
        );
    }
}

fn drivers_backup(app: &mut App, ui: &mut Ui) {
    ui.label("Export all third-party driver packages with 'pnputil /export-driver'. Requires administrator rights.");
    ui.horizontal(|ui| {
        ui.label("Folder");
        ui.add(egui::TextEdit::singleline(&mut app.drivers.backup_dir).desired_width(420.0));
        if ui.button("Browse...").clicked() {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                app.drivers.backup_dir = p.to_string_lossy().into_owned();
            }
        }
    });
    if ui
        .add_enabled(
            !app.drivers.backing_up && !app.drivers.backup_dir.trim().is_empty(),
            primary_btn(ui, "Back up drivers"),
        )
        .clicked()
    {
        let dir = std::path::PathBuf::from(app.drivers.backup_dir.trim());
        app.ask(
            "Back up drivers",
            format!("Export all third-party drivers to:\n{}", dir.display()),
            if app.elevated {
                None
            } else {
                Some("Administrator rights are required.".into())
            },
            Act::BackupDrivers(dir),
        );
    }
    if app.drivers.backing_up {
        ui.spinner();
    }
    if let Some(r) = &app.drivers.backup_result {
        ui.label(RichText::new(r).strong());
    }
    egui::ScrollArea::vertical()
        .max_height(300.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for l in &app.drivers.backup_log {
                ui.label(RichText::new(l).monospace().small());
            }
        });
}

fn restore_points(app: &mut App, ui: &mut Ui) {
    ui.label("System Restore points on this PC. Keep the ones you may want to roll back to; deleting one is permanent.");
    if !app.elevated {
        ui.add_space(6.0);
        ui.label("Viewing and deleting restore points needs administrator rights (use \"Restart as admin\").");
        return;
    }
    if !app.drivers.rp_loaded && !app.drivers.rp_loading {
        app.load_restore_points();
    }
    ui.horizontal(|ui| {
        if ui.button(tr(Lang::English, "common.refresh")).clicked() {
            app.load_restore_points();
        }
        if app.drivers.rp_loading {
            ui.spinner();
        }
    });
    if let Some(e) = &app.drivers.rp_error {
        ui.colored_label(Color32::from_rgb(0xe5, 0x48, 0x4d), e);
        return;
    }
    if app.drivers.rp_loaded && app.drivers.restore_points.is_empty() {
        ui.label("No restore points exist (or System Protection is turned off).");
        return;
    }
    let mut delete: Option<(u32, String, bool)> = None;
    let newest = app.drivers.restore_points.first().map(|r| r.sequence);
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .column(Column::exact(50.0))
        .column(Column::initial(140.0))
        .column(Column::initial(170.0))
        .column(Column::remainder().clip(true))
        .column(Column::exact(90.0))
        .header(ROW_H, |mut h| {
            for t in ["#", "Created (UTC)", "Type", "Description", ""] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            let n = app.drivers.restore_points.len();
            body.rows(ROW_H, n, |mut row| {
                let r = &app.drivers.restore_points[row.index()];
                row.col(|ui| {
                    ui.label(r.sequence.to_string());
                });
                row.col(|ui| {
                    ui.label(&r.created);
                });
                row.col(|ui| {
                    ui.label(&r.kind);
                });
                row.col(|ui| {
                    ui.label(&r.description).on_hover_text(&r.description);
                });
                row.col(|ui| {
                    if ui.add(danger_btn(ui, "Delete")).clicked() {
                        delete = Some((
                            r.sequence,
                            r.description.clone(),
                            Some(r.sequence) == newest,
                        ));
                    }
                });
            });
        });
    if let Some((seq, desc, is_newest)) = delete {
        let mut warn = "Deleting a restore point cannot be undone. You will no longer be able to roll back to it.".to_string();
        if is_newest {
            warn.push_str("\nThis is your NEWEST restore point.");
        }
        app.ask(
            "Delete restore point",
            format!("Permanently delete restore point #{seq} \"{desc}\"?"),
            Some(warn),
            Act::DeleteRestorePoint(seq, desc),
        );
    }
}

fn drivers_gpu(app: &mut App, ui: &mut Ui) {
    let gpus = detect_gpus(&app.drivers.installed);
    ui.label("Rustacer never downloads or installs GPU drivers. Use the vendor's official page.");
    ui.add_space(6.0);
    if gpus.is_empty() {
        ui.label("No NVIDIA, AMD or Intel GPU was detected.");
    }
    let mut open: Option<GpuVendor> = None;
    for g in &gpus {
        ui.group(|ui| {
            ui.label(RichText::new(&g.name).strong());
            ui.label(format!(
                "Vendor: {}   Installed version: {}   Date: {}",
                g.vendor.label(),
                g.version,
                g.date
            ));
            if ui
                .button(format!("Open official {} driver page", g.vendor.label()))
                .clicked()
            {
                open = Some(g.vendor);
            }
        });
    }
    if let Some(v) = open {
        app.open_vendor(v);
    }
}

// ---------------- services ----------------

fn services_page(app: &mut App, ui: &mut Ui) {
    heading(ui, "Services");
    ui.horizontal(|ui| {
        search_box(ui, &mut app.services.filter);
        if ui.button(tr(Lang::English, "common.refresh")).clicked() {
            app.load_page(Page::Services, true);
        }
        if app.services.loading {
            ui.spinner();
        }
    });
    let f = app.services.filter.to_lowercase();
    let rows: Vec<usize> = app
        .services
        .list
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            f.is_empty()
                || s.name.to_lowercase().contains(&f)
                || s.display_name.to_lowercase().contains(&f)
                || s.description.to_lowercase().contains(&f)
        })
        .map(|(i, _)| i)
        .collect();
    let mut act: Option<(String, SvcAction)> = None;
    if let Some(sel) = app.services.selected.clone() {
        if let Some(s) = app.services.list.iter().find(|s| s.name == sel) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&s.display_name).strong());
                if services::is_protected(&s.name) {
                    ui.colored_label(Color32::from_rgb(0xe5, 0x48, 0x4d), "protected");
                } else if services::is_critical(&s.name) {
                    ui.colored_label(Color32::from_rgb(0xf5, 0xa6, 0x23), "critical");
                }
                if ui.button("Start").clicked() {
                    act = Some((s.name.clone(), SvcAction::Start));
                }
                if ui.button("Stop").clicked() {
                    act = Some((s.name.clone(), SvcAction::Stop));
                }
                if ui.button("Restart").clicked() {
                    act = Some((s.name.clone(), SvcAction::Restart));
                }
                egui::ComboBox::from_id_salt("starttype")
                    .selected_text(format!("Start type: {}", s.start_type.label()))
                    .show_ui(ui, |ui| {
                        for t in StartType::SETTABLE {
                            if ui.selectable_label(s.start_type == t, t.label()).clicked()
                                && s.start_type != t
                            {
                                act = Some((s.name.clone(), SvcAction::SetStartType(t)));
                            }
                        }
                    });
            });
        }
    }
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .column(Column::initial(170.0).clip(true))
        .column(Column::initial(250.0).clip(true))
        .column(Column::initial(80.0))
        .column(Column::initial(130.0))
        .column(Column::remainder().clip(true))
        .header(ROW_H, |mut h| {
            for t in [
                "Name",
                "Display name",
                "Status",
                "Start type",
                "Description",
            ] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let s = &app.services.list[rows[row.index()]];
                row.set_selected(app.services.selected.as_deref() == Some(s.name.as_str()));
                row.col(|ui| {
                    ui.label(&s.name);
                });
                row.col(|ui| {
                    ui.label(&s.display_name);
                });
                row.col(|ui| {
                    let c = if s.state == ServiceState::Running {
                        Color32::from_rgb(0x3d, 0xb8, 0x6b)
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.colored_label(c, s.state.label());
                });
                row.col(|ui| {
                    ui.label(s.start_type.label());
                });
                row.col(|ui| {
                    ui.label(&s.description);
                });
                if row.response().clicked() {
                    app.services.selected = Some(s.name.clone());
                }
            });
        });
    if let Some((name, a)) = act {
        let busy = app.drivers.scanning || app.drivers.installing;
        match services::check_change(&name, a, busy) {
            Err(e) => app.notify(true, e.to_string()),
            Ok(()) => {
                let warn = services::is_critical(&name).then(|| "This is a critical Windows service. Changing it may affect system stability.".to_string());
                let what = match a {
                    SvcAction::Start => "Start".to_string(),
                    SvcAction::Stop => "Stop".to_string(),
                    SvcAction::Restart => "Restart".to_string(),
                    SvcAction::SetStartType(t) => format!("Set start type to {} for", t.label()),
                };
                app.ask(
                    "Change service",
                    format!("{what} service '{name}'?"),
                    warn,
                    Act::Service(name, a),
                );
            }
        }
    }
}

// ---------------- processes ----------------

fn processes(app: &mut App, ui: &mut Ui) {
    heading(ui, "Processes");
    app.procs_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    search_box(ui, &mut app.procs.filter);
    let f = app.procs.filter.to_lowercase();
    let mut rows: Vec<usize> = app
        .procs
        .list
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            f.is_empty() || p.name.to_lowercase().contains(&f) || p.pid.to_string() == f
        })
        .map(|(i, _)| i)
        .collect();
    let (col, desc) = (app.procs.sort_col, app.procs.sort_desc);
    {
        let l = &app.procs.list;
        rows.sort_by(|&a, &b| {
            let (x, y) = (&l[a], &l[b]);
            let o = match col {
                0 => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                1 => x.pid.cmp(&y.pid),
                2 => x
                    .cpu
                    .partial_cmp(&y.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal),
                3 => x.memory.cmp(&y.memory),
                4 => x.user.cmp(&y.user),
                _ => x.path.cmp(&y.path),
            };
            if desc {
                o.reverse()
            } else {
                o
            }
        });
    }
    let sel = app
        .procs
        .selected
        .and_then(|pid| app.procs.list.iter().find(|p| p.pid == pid))
        .cloned();
    let mut kill: Option<(bool, u32, String)> = None;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(sel.is_some(), |ui| {
            if ui.add(danger_btn(ui, "End process")).clicked() {
                if let Some(p) = &sel {
                    kill = Some((false, p.pid, p.name.clone()));
                }
            }
            if ui.add(danger_btn(ui, "End process tree")).clicked() {
                if let Some(p) = &sel {
                    kill = Some((true, p.pid, p.name.clone()));
                }
            }
            if ui.button("Open file location").clicked() {
                if let Some(path) = sel.as_ref().and_then(|p| p.path.clone()) {
                    app.spawn(move |bus| {
                        if let Err(e) = reveal_in_explorer(&path) {
                            bus.err(e.to_string());
                        }
                    });
                }
            }
        });
        ui.label(RichText::new(format!("{} processes", rows.len())).weak());
    });
    let heads = ["Name", "PID", "CPU %", "Memory", "User", "Path"];
    let mut new_sort: Option<usize> = None;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .column(Column::initial(200.0).clip(true))
        .column(Column::initial(60.0))
        .column(Column::initial(70.0))
        .column(Column::initial(90.0))
        .column(Column::initial(140.0).clip(true))
        .column(Column::remainder().clip(true))
        .header(ROW_H, |mut h| {
            for (i, t) in heads.iter().enumerate() {
                h.col(|ui| {
                    let arrow = if app.procs.sort_col == i {
                        if app.procs.sort_desc {
                            " v"
                        } else {
                            " ^"
                        }
                    } else {
                        ""
                    };
                    if ui
                        .add(
                            egui::Button::new(RichText::new(format!("{t}{arrow}")).strong())
                                .frame(false),
                        )
                        .clicked()
                    {
                        new_sort = Some(i);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let p = &app.procs.list[rows[row.index()]];
                row.set_selected(app.procs.selected == Some(p.pid));
                row.col(|ui| {
                    ui.label(&p.name);
                });
                row.col(|ui| {
                    ui.label(p.pid.to_string());
                });
                row.col(|ui| {
                    ui.label(format!("{:.1}", p.cpu));
                });
                row.col(|ui| {
                    ui.label(format_bytes(p.memory));
                });
                row.col(|ui| {
                    ui.label(&p.user);
                });
                row.col(|ui| {
                    ui.label(
                        p.path
                            .as_ref()
                            .map(|x| x.display().to_string())
                            .unwrap_or_default(),
                    );
                });
                if row.response().clicked() {
                    app.procs.selected = Some(p.pid);
                }
            });
        });
    if let Some(i) = new_sort {
        if app.procs.sort_col == i {
            app.procs.sort_desc = !app.procs.sort_desc;
        } else {
            app.procs.sort_col = i;
            app.procs.sort_desc = (2..=3).contains(&i);
        }
    }
    if let Some((tree, pid, name)) = kill {
        if rustacer_core::processes::is_critical(&name) {
            app.notify(
                true,
                format!("'{name}' is a critical system process and cannot be ended."),
            );
        } else if tree {
            app.ask(
                "End process tree",
                format!("End '{name}' (PID {pid}) and all child processes?"),
                Some("Unsaved data in these processes will be lost.".into()),
                Act::KillTree(pid, name),
            );
        } else {
            app.ask(
                "End process",
                format!("End '{name}' (PID {pid})?"),
                Some("Unsaved data in this process will be lost.".into()),
                Act::KillProcess(pid, name),
            );
        }
    }
}

// ---------------- uninstaller ----------------

fn uninstaller(app: &mut App, ui: &mut Ui) {
    heading(ui, "Uninstaller");
    ui.horizontal(|ui| {
        search_box(ui, &mut app.uninst.filter);
        ui.checkbox(
            &mut app.uninst.hide_system,
            "Hide system components and updates",
        );
        if ui.button(tr(Lang::English, "common.refresh")).clicked() {
            app.load_page(Page::Uninstaller, true);
        }
        if app.uninst.loading {
            ui.spinner();
        }
    });
    if !app.uninst.winget_probed {
        app.uninst.winget_probed = true;
        app.spawn(|bus| {
            bus.send(Msg::WingetAvailable(
                rustacer_core::uninstaller::winget_available(),
            ))
        });
    }
    if app.uninst.winget_ok == Some(true) {
        let mut want = app.uninst.show_winget;
        if ui
            .checkbox(&mut want, "Show winget-installed apps")
            .changed()
        {
            app.uninst.show_winget = want;
            if want && app.uninst.winget.is_empty() {
                app.spawn(|bus| match rustacer_core::uninstaller::list_winget() {
                    Ok(v) => bus.send(Msg::Winget(v)),
                    Err(e) => bus.err(e.to_string()),
                });
            }
        }
    }
    if app.uninst.show_winget {
        winget_table(app, ui);
        return;
    }
    let f = app.uninst.filter.to_lowercase();
    let hide = app.uninst.hide_system;
    let rows: Vec<usize> = app
        .uninst
        .list
        .iter()
        .enumerate()
        .filter(|(_, a)| !(hide && (a.is_system_component || a.is_update)))
        .filter(|(_, a)| {
            f.is_empty()
                || a.name.to_lowercase().contains(&f)
                || a.publisher.to_lowercase().contains(&f)
        })
        .map(|(i, _)| i)
        .collect();
    let sel = app
        .uninst
        .selected
        .and_then(|i| app.uninst.list.get(i))
        .cloned();
    let mut act: Option<(AppEntryOwned, bool)> = None;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(sel.is_some(), |ui| {
            if ui.add(primary_btn(ui, "Uninstall")).clicked() {
                if let Some(a) = &sel {
                    act = Some((a.clone(), false));
                }
            }
            let has_quiet = sel
                .as_ref()
                .is_some_and(|a| !a.quiet_uninstall_string.is_empty());
            if ui
                .add_enabled(has_quiet, egui::Button::new("Quiet uninstall"))
                .clicked()
            {
                if let Some(a) = &sel {
                    act = Some((a.clone(), true));
                }
            }
        });
        ui.label(RichText::new(format!("{} apps", rows.len())).weak());
    });
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .column(Column::initial(280.0).clip(true))
        .column(Column::initial(180.0).clip(true))
        .column(Column::initial(110.0).clip(true))
        .column(Column::initial(80.0))
        .column(Column::remainder())
        .header(ROW_H, |mut h| {
            for t in ["Name", "Publisher", "Version", "Size", "Installed"] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let i = rows[row.index()];
                let a = &app.uninst.list[i];
                row.set_selected(app.uninst.selected == Some(i));
                row.col(|ui| {
                    ui.label(&a.name);
                });
                row.col(|ui| {
                    ui.label(&a.publisher);
                });
                row.col(|ui| {
                    ui.label(&a.version);
                });
                row.col(|ui| {
                    ui.label(if a.size > 0 {
                        format_bytes(a.size)
                    } else {
                        String::new()
                    });
                });
                row.col(|ui| {
                    ui.label(&a.install_date);
                });
                if row.response().clicked() {
                    app.uninst.selected = Some(i);
                }
            });
        });
    if let Some((a, quiet)) = act {
        app.ask(
            "Uninstall application",
            format!(
                "Run the {}uninstaller for '{}'?",
                if quiet { "quiet " } else { "" },
                a.name
            ),
            Some("The application's own uninstaller will run; Rustacer cannot undo it.".into()),
            Act::Uninstall(a, quiet),
        );
    }
}

type AppEntryOwned = rustacer_core::uninstaller::AppEntry;

fn winget_table(app: &mut App, ui: &mut Ui) {
    let f = app.uninst.filter.to_lowercase();
    let rows: Vec<usize> = app
        .uninst
        .winget
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            f.is_empty() || w.name.to_lowercase().contains(&f) || w.id.to_lowercase().contains(&f)
        })
        .map(|(i, _)| i)
        .collect();
    let sel = app
        .uninst
        .winget_selected
        .and_then(|i| app.uninst.winget.get(i))
        .cloned();
    if ui
        .add_enabled(sel.is_some(), primary_btn(ui, "Uninstall with winget"))
        .clicked()
    {
        if let Some(w) = sel {
            app.ask(
                "winget uninstall",
                format!("Uninstall '{}' ({}) using winget?", w.name, w.id),
                None,
                Act::WingetUninstall(w),
            );
        }
    }
    TableBuilder::new(ui)
        .striped(true)
        .sense(Sense::click())
        .column(Column::initial(300.0).clip(true))
        .column(Column::initial(240.0).clip(true))
        .column(Column::remainder())
        .header(ROW_H, |mut h| {
            for t in ["Name", "Id", "Version"] {
                h.col(|ui| {
                    ui.strong(t);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, rows.len(), |mut row| {
                let i = rows[row.index()];
                let w = &app.uninst.winget[i];
                row.set_selected(app.uninst.winget_selected == Some(i));
                row.col(|ui| {
                    ui.label(&w.name);
                });
                row.col(|ui| {
                    ui.label(&w.id);
                });
                row.col(|ui| {
                    ui.label(&w.version);
                });
                if row.response().clicked() {
                    app.uninst.winget_selected = Some(i);
                }
            });
        });
}

// ---------------- resources ----------------

fn resources(app: &mut App, ui: &mut Ui) {
    heading(ui, "Resources");
    ui.horizontal(|ui| {
        for (label, secs) in [("60 s", 60.0), ("5 min", 300.0), ("30 min", 1800.0)] {
            if ui.selectable_label(app.res_range == secs, label).clicked() {
                app.res_range = secs;
            }
        }
    });
    let now = app.started.elapsed().as_secs_f64();
    let range = app.res_range;
    let pts = |f: &dyn Fn(&Sample) -> f64| -> PlotPoints<'static> {
        app.history
            .iter()
            .filter(|s| now - s.t <= range)
            .map(|s| [s.t - now, f(s)])
            .collect::<Vec<_>>()
            .into()
    };
    let plot = |ui: &mut Ui, id: &str, title: &str, pct: bool, lines: Vec<Line<'static>>| {
        ui.label(RichText::new(title).strong());
        let mut p = Plot::new(id.to_string())
            .height(150.0)
            .allow_zoom(false)
            .allow_drag(false)
            .allow_scroll(false)
            .include_x(-range)
            .include_x(0.0)
            .include_y(0.0);
        // A legend with one entry per core overflows the plot; hovering names the line instead.
        if lines.len() <= 4 {
            p = p.legend(egui_plot::Legend::default());
        }
        if pct {
            p = p.include_y(100.0);
        }
        p.show(ui, |pu| {
            for l in lines {
                pu.line(l);
            }
        });
    };
    let mb = 1024.0 * 1024.0;
    egui::ScrollArea::vertical().show(ui, |ui| {
        let ncores = app.history.back().map(|s| s.cores.len()).unwrap_or(0);
        let core_lines: Vec<Line<'static>> = (0..ncores)
            .map(|c| {
                Line::new(
                    format!("Core {c}"),
                    pts(&|s: &Sample| s.cores.get(c).copied().unwrap_or(0.0) as f64),
                )
            })
            .collect();
        plot(ui, "cpu", "CPU per core (%)", true, core_lines);
        plot(
            ui,
            "mem",
            "Memory and swap/pagefile (%)",
            true,
            vec![
                Line::new("RAM", pts(&|s: &Sample| s.ram as f64)),
                Line::new("Swap", pts(&|s: &Sample| s.swap as f64)),
            ],
        );
        plot(
            ui,
            "disk",
            "Disk I/O (MB/s)",
            false,
            vec![
                Line::new("Read", pts(&|s: &Sample| s.disk_r / mb)),
                Line::new("Write", pts(&|s: &Sample| s.disk_w / mb)),
            ],
        );
        plot(
            ui,
            "net",
            "Network (MB/s)",
            false,
            vec![
                Line::new("Down", pts(&|s: &Sample| s.net_d / mb)),
                Line::new("Up", pts(&|s: &Sample| s.net_u / mb)),
            ],
        );
    });
}

// ---------------- settings ----------------

fn settings(app: &mut App, ui: &mut Ui) {
    heading(ui, "Settings");
    let mut changed = false;
    egui::Grid::new("settings")
        .num_columns(2)
        .spacing([24.0, 10.0])
        .show(ui, |ui| {
            ui.label("Theme");
            ui.horizontal_wrapped(|ui| {
                for t in Theme::ALL {
                    changed |= ui
                        .selectable_value(&mut app.cfg.theme, t, t.label())
                        .changed();
                }
            });
            ui.end_row();
            ui.label("Refresh interval (ms)");
            changed |= ui
                .add(egui::Slider::new(
                    &mut app.cfg.refresh_interval_ms,
                    250..=5000,
                ))
                .changed();
            ui.end_row();
            ui.label("Alerts (toast notifications)");
            changed |= ui
                .checkbox(&mut app.cfg.alerts_enabled, "Enabled")
                .changed();
            ui.end_row();
            ui.label("CPU alert threshold (%)");
            changed |= ui
                .add(egui::Slider::new(
                    &mut app.cfg.cpu_alert_percent,
                    10.0..=100.0,
                ))
                .changed();
            ui.end_row();
            ui.label("RAM alert threshold (%)");
            changed |= ui
                .add(egui::Slider::new(
                    &mut app.cfg.ram_alert_percent,
                    10.0..=100.0,
                ))
                .changed();
            ui.end_row();
            ui.label("Disk alert threshold (%)");
            changed |= ui
                .add(egui::Slider::new(
                    &mut app.cfg.disk_alert_percent,
                    10.0..=100.0,
                ))
                .changed();
            ui.end_row();
            ui.label("\"Old driver\" age (years)");
            changed |= ui
                .add(egui::Slider::new(&mut app.cfg.old_driver_years, 1..=15))
                .changed();
            ui.end_row();
            ui.label("Drivers");
            changed |= ui
                .checkbox(
                    &mut app.cfg.check_drivers_on_startup,
                    "Check for driver updates on startup (scan only, never installs)",
                )
                .changed();
            ui.end_row();
            ui.label("Start with Windows");
            let mut sw = app.self_registered;
            if ui
                .checkbox(&mut sw, "Launch Rustacer when I sign in")
                .changed()
            {
                match rustacer_core::startup::set_self_registered(sw) {
                    Ok(()) => {
                        app.self_registered = sw;
                        app.cfg.start_with_windows = sw;
                        changed = true;
                    }
                    Err(e) => app.notify(true, e.to_string()),
                }
            }
            ui.end_row();
        });
    if changed {
        app.save_cfg();
    }
    ui.add_space(16.0);
    ui.separator();
    ui.label(format!("Rustacer {} - MIT licensed. Inspired by Stacer (a Linux system optimizer); no code or assets were copied.", env!("CARGO_PKG_VERSION")));
    ui.horizontal(|ui| {
        if let Some(d) = rustacer_core::config::log_dir() {
            if ui.button("Open log folder").clicked() {
                app.spawn(move |bus| {
                    if let Err(e) = open_path(&d.to_string_lossy()) {
                        bus.err(e.to_string());
                    }
                });
            }
        }
    });
}
