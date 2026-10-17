//! Installed-application listing and uninstall launching.

use crate::error::{CoreError, Result};
use crate::util::{
    run_hidden, run_hidden_cancellable, shell_execute, shell_execute_wait, split_command_line,
};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use tracing::info;
use winreg::enums::*;
use winreg::RegKey;

#[derive(Debug, Clone, Default)]
pub struct AppEntry {
    pub key_name: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    /// Bytes (the registry stores KB).
    pub size: u64,
    /// ISO-ish "YYYY-MM-DD" or empty.
    pub install_date: String,
    pub uninstall_string: String,
    pub quiet_uninstall_string: String,
    pub is_system_component: bool,
    pub is_update: bool,
    pub source: String,
}

/// Raw registry values for one Uninstall subkey (mockable).
#[derive(Debug, Clone, Default)]
pub struct RawApp {
    pub strings: HashMap<String, String>,
    pub dwords: HashMap<String, u32>,
}

pub fn format_install_date(raw: &str) -> String {
    let r = raw.trim();
    if r.len() == 8 && r.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &r[0..4], &r[4..6], &r[6..8])
    } else {
        String::new()
    }
}

/// Convert raw values to an entry; returns None for keys without a display name.
pub fn parse_entry(key_name: &str, raw: &RawApp, source: &str) -> Option<AppEntry> {
    let get = |k: &str| {
        raw.strings
            .get(k)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let name = get("DisplayName");
    if name.is_empty() {
        return None;
    }
    let release = get("ReleaseType").to_ascii_lowercase();
    let is_update = !get("ParentKeyName").is_empty()
        || release.contains("update")
        || release.contains("hotfix")
        || key_name.to_ascii_uppercase().starts_with("KB")
        || name.starts_with("Update for ")
        || name.starts_with("Security Update for ")
        || name.starts_with("Hotfix for ");
    Some(AppEntry {
        key_name: key_name.to_string(),
        name,
        publisher: get("Publisher"),
        version: get("DisplayVersion"),
        size: raw.dwords.get("EstimatedSize").copied().unwrap_or(0) as u64 * 1024,
        install_date: format_install_date(&get("InstallDate")),
        uninstall_string: get("UninstallString"),
        quiet_uninstall_string: get("QuietUninstallString"),
        is_system_component: raw.dwords.get("SystemComponent").copied().unwrap_or(0) == 1,
        is_update,
        source: source.to_string(),
    })
}

fn read_hive(root: RegKey, path: &str, source: &str, out: &mut Vec<AppEntry>) {
    let Ok(key) = root.open_subkey(path) else {
        return;
    };
    for sub in key.enum_keys().flatten() {
        let Ok(k) = key.open_subkey(&sub) else {
            continue;
        };
        let mut raw = RawApp::default();
        for (n, _) in k.enum_values().flatten() {
            if let Ok(s) = k.get_value::<String, _>(&n) {
                raw.strings.insert(n.clone(), s);
            } else if let Ok(d) = k.get_value::<u32, _>(&n) {
                raw.dwords.insert(n, d);
            }
        }
        if let Some(e) = parse_entry(&sub, &raw, source) {
            out.push(e);
        }
    }
}

pub fn list_installed() -> Vec<AppEntry> {
    const P: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const P32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    let mut out = Vec::new();
    read_hive(RegKey::predef(HKEY_LOCAL_MACHINE), P, "HKLM", &mut out);
    read_hive(
        RegKey::predef(HKEY_LOCAL_MACHINE),
        P32,
        "HKLM (32-bit)",
        &mut out,
    );
    read_hive(RegKey::predef(HKEY_CURRENT_USER), P, "HKCU", &mut out);
    dedupe(out)
}

/// The same product can be registered in several hives (per-user and machine, or the
/// 32- and 64-bit views). Keep one row per name+version+publisher, preferring the entry
/// that has an uninstall command.
pub fn dedupe(mut v: Vec<AppEntry>) -> Vec<AppEntry> {
    v.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| {
                a.uninstall_string
                    .is_empty()
                    .cmp(&b.uninstall_string.is_empty())
            })
    });
    let mut seen = std::collections::HashSet::new();
    v.retain(|a| {
        seen.insert((
            a.name.to_lowercase(),
            a.version.to_lowercase(),
            a.publisher.to_lowercase(),
        ))
    });
    v
}

/// Launch the uninstaller. ShellExecute lets the installer request UAC itself.
pub fn uninstall(app: &AppEntry, quiet: bool) -> Result<()> {
    let cmd = if quiet && !app.quiet_uninstall_string.is_empty() {
        &app.quiet_uninstall_string
    } else {
        &app.uninstall_string
    };
    if cmd.trim().is_empty() {
        return Err(CoreError::Message(format!(
            "'{}' has no uninstall command.",
            app.name
        )));
    }
    let (prog, args) = split_command_line(cmd);
    info!("launching uninstaller for '{}': {cmd}", app.name);
    shell_execute(
        "open",
        &prog,
        if args.is_empty() { None } else { Some(&args) },
    )
}

// ---------- winget (optional) ----------

#[derive(Debug, Clone)]
pub struct WingetApp {
    pub name: String,
    pub id: String,
    pub version: String,
}

pub fn winget_available() -> bool {
    run_hidden("winget", &["--version"])
        .map(|(c, _)| c == 0)
        .unwrap_or(false)
}

pub fn parse_winget_list(text: &str) -> Vec<WingetApp> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(h) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("Name") && l.contains("Id"))
    else {
        return vec![];
    };
    let header: Vec<char> = lines[h].chars().collect();
    let col = |label: &str| lines[h].find(label).map(|b| lines[h][..b].chars().count());
    let (Some(id_c), Some(ver_c)) = (col("Id"), col("Version")) else {
        return vec![];
    };
    let end_ver = col("Available")
        .or_else(|| col("Source"))
        .unwrap_or(header.len().max(ver_c + 1));
    let slice = |l: &str, a: usize, b: usize| -> String {
        l.chars()
            .skip(a)
            .take(b.saturating_sub(a))
            .collect::<String>()
            .trim()
            .to_string()
    };
    lines
        .iter()
        .skip(h + 2)
        .filter(|l| !l.trim().is_empty())
        .map(|l| WingetApp {
            name: slice(l, 0, id_c),
            id: slice(l, id_c, ver_c),
            version: slice(l, ver_c, end_ver),
        })
        .filter(|a| !a.id.is_empty())
        .collect()
}

pub fn list_winget() -> Result<Vec<WingetApp>> {
    let (code, out) = run_hidden(
        "winget",
        &[
            "list",
            "--disable-interactivity",
            "--accept-source-agreements",
        ],
    )?;
    if code != 0 {
        return Err(CoreError::Message("winget list failed.".into()));
    }
    Ok(parse_winget_list(&out))
}

/// A winget package id is passed to winget as a single argument, so it must not contain
/// anything that could split or alter the command line. An id winget cut short with "..."
/// (U+2026) on a narrow console is refused too: it would not match the package.
pub fn validate_winget_id(id: &str) -> Result<()> {
    let bad = id.is_empty()
        || id.starts_with('-')
        || id
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"&|<>^%\u{2026}".contains(c));
    if bad {
        Err(CoreError::Blocked("Invalid winget package id.".into()))
    } else {
        Ok(())
    }
}

pub fn winget_uninstall(id: &str) -> Result<()> {
    validate_winget_id(id)?;
    info!("winget uninstall {id}");
    shell_execute(
        "open",
        "winget",
        Some(&format!("uninstall --id {id} --exact")),
    )
}

// ---------- winget upgrades ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WingetUpgrade {
    pub name: String,
    pub id: String,
    pub version: String,
    pub available: String,
    pub source: String,
}

/// Why a package is not offered for upgrade, if it is a GPU driver package. Display drivers
/// are only ever updated from the vendor's own page (see the Driver Updater GPU tab).
/// Matches on the winget id and the name for NVIDIA graphics drivers, AMD Software / Adrenalin
/// and Intel graphics drivers.
pub fn gpu_driver_reason(name: &str, id: &str) -> Option<&'static str> {
    let hay = format!("{name} {id}").to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| hay.contains(n));
    if hay.contains("nvidia")
        && has(&[
            "driver",
            "geforce",
            "graphics",
            "display",
            "nvidia app",
            "nvidiaapp",
            "game ready",
        ])
    {
        return Some("NVIDIA graphics driver");
    }
    if has(&["adrenalin", "radeon"])
        || (hay.contains("amd")
            && has(&["graphics", "display driver", "amd software", "amdsoftware"]))
    {
        return Some("AMD graphics driver / Adrenalin");
    }
    if hay.contains("intel")
        && has(&[
            "graphics",
            "uhd",
            "iris",
            "intel arc",
            "arc control",
            "display driver",
            "driver & support",
            "driver and support",
            "driversupport",
        ])
    {
        return Some("Intel graphics driver");
    }
    None
}

/// Result of an upgrade check.
#[derive(Debug, Clone, Default)]
pub struct UpgradeList {
    /// Packages offered to the user.
    pub items: Vec<WingetUpgrade>,
    /// GPU driver packages that were left out, with the reason.
    pub excluded: Vec<(WingetUpgrade, &'static str)>,
    /// winget said nothing is available, or similar.
    pub note: Option<String>,
}

/// Split parsed rows into offered packages and excluded GPU driver packages.
pub fn split_upgrades(all: Vec<WingetUpgrade>) -> UpgradeList {
    let mut list = UpgradeList::default();
    for u in all {
        match gpu_driver_reason(&u.name, &u.id) {
            Some(why) => list.excluded.push((u, why)),
            None => list.items.push(u),
        }
    }
    list
}

fn is_separator(l: &str) -> bool {
    let t = l.trim();
    t.len() >= 5 && t.chars().all(|c| c == '-')
}

fn is_footer(l: &str) -> bool {
    let t = l.trim().to_ascii_lowercase();
    t.is_empty()
        || t.starts_with("the following")
        || t.starts_with("no installed package")
        || t.starts_with("no applicable")
        || (t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains("upgrade"))
}

/// Parse the table printed by `winget upgrade`, by column position like [`parse_winget_list`].
/// Handles several tables (winget prints a second one for packages that need explicit
/// targeting), spinner noise before the header, and the "N upgrades available." footer.
pub fn parse_winget_upgrade(text: &str) -> Vec<WingetUpgrade> {
    // winget redraws a spinner with carriage returns; only the last segment of a line counts.
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or(""))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let h = lines[i];
        let is_header = h.trim_start().starts_with("Name")
            && h.contains("Id")
            && h.contains("Version")
            && h.contains("Available");
        if !is_header {
            i += 1;
            continue;
        }
        let col = |label: &str| h.find(label).map(|b| h[..b].chars().count());
        let (Some(id_c), Some(ver_c), Some(av_c)) = (col("Id"), col("Version"), col("Available"))
        else {
            i += 1;
            continue;
        };
        let src_c = col("Source");
        let slice = |l: &str, a: usize, b: Option<usize>| -> String {
            let it = l.chars().skip(a);
            match b {
                Some(b) => it.take(b.saturating_sub(a)).collect::<String>(),
                None => it.collect::<String>(),
            }
            .trim()
            .to_string()
        };
        i += 1;
        if i < lines.len() && is_separator(lines[i]) {
            i += 1;
        }
        while i < lines.len() && !is_footer(lines[i]) {
            let l = lines[i];
            let u = WingetUpgrade {
                name: slice(l, 0, Some(id_c)),
                id: slice(l, id_c, Some(ver_c)),
                version: slice(l, ver_c, Some(av_c)),
                available: slice(l, av_c, src_c),
                source: src_c.map(|s| slice(l, s, None)).unwrap_or_default(),
            };
            if !u.id.is_empty() && !u.available.is_empty() {
                out.push(u);
            }
            i += 1;
        }
    }
    out
}

/// Run `winget upgrade` (listing only) and parse it. Cancellable; call from a worker thread.
pub fn list_upgrades(cancel: &AtomicBool) -> Result<UpgradeList> {
    let (code, out) = run_hidden_cancellable(
        "winget",
        &[
            "upgrade",
            "--include-unknown",
            "--disable-interactivity",
            "--accept-source-agreements",
        ],
        cancel,
    )?;
    let rows = parse_winget_upgrade(&out);
    if rows.is_empty() {
        let lower = out.to_ascii_lowercase();
        if code != 0 && !lower.contains("no installed package") && !lower.contains("no applicable")
        {
            let tail: String = out
                .trim()
                .chars()
                .rev()
                .take(300)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(CoreError::Message(format!(
                "winget upgrade failed (exit code {code}). {tail}"
            )));
        }
        return Ok(UpgradeList {
            note: Some("winget reports that no application updates are available.".into()),
            ..Default::default()
        });
    }
    Ok(split_upgrades(rows))
}

/// Upgrade one package with `winget upgrade --id <id> --exact` through ShellExecute, so
/// winget's own window and any UAC prompt are shown. Waits for it to finish and returns the
/// exit code (`None` if Windows gave no process handle). No silent or force flags are passed.
pub fn winget_upgrade(id: &str) -> Result<Option<u32>> {
    validate_winget_id(id)?;
    if let Some((_, why)) = gpu_driver_reason("", id).map(|w| ((), w)) {
        return Err(CoreError::Blocked(format!(
            "'{id}' is a {why} package; Rustacer never installs GPU drivers."
        )));
    }
    info!("winget upgrade --id {id} --exact");
    shell_execute_wait(
        "open",
        "winget",
        Some(&format!("upgrade --id {id} --exact")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(strings: &[(&str, &str)], dwords: &[(&str, u32)]) -> RawApp {
        RawApp {
            strings: strings
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            dwords: dwords.iter().map(|(a, b)| (a.to_string(), *b)).collect(),
        }
    }

    #[test]
    fn parses_basic_app() {
        let r = raw(
            &[
                ("DisplayName", "7-Zip 23.01"),
                ("Publisher", "Igor Pavlov"),
                ("DisplayVersion", "23.01"),
                ("InstallDate", "20240315"),
                ("UninstallString", "\"C:\\7z\\Uninstall.exe\""),
            ],
            &[("EstimatedSize", 5120)],
        );
        let e = parse_entry("7-Zip", &r, "HKLM").unwrap();
        assert_eq!(e.size, 5120 * 1024);
        assert_eq!(e.install_date, "2024-03-15");
        assert!(!e.is_update && !e.is_system_component);
    }

    #[test]
    fn flags_updates_and_components() {
        let u = raw(
            &[
                ("DisplayName", "Update for X"),
                ("ParentKeyName", "OperatingSystem"),
            ],
            &[],
        );
        assert!(parse_entry("KB123", &u, "HKLM").unwrap().is_update);
        let s = raw(&[("DisplayName", "Runtime")], &[("SystemComponent", 1)]);
        assert!(parse_entry("rt", &s, "HKLM").unwrap().is_system_component);
    }

    #[test]
    fn dedupes_across_hives() {
        let mk = |src: &str, cmd: &str| AppEntry {
            name: "App".into(),
            version: "1.0".into(),
            publisher: "Pub".into(),
            uninstall_string: cmd.into(),
            source: src.into(),
            ..Default::default()
        };
        let other = AppEntry {
            name: "App".into(),
            version: "2.0".into(),
            ..Default::default()
        };
        let v = dedupe(vec![
            mk("HKCU", ""),
            mk("HKLM", "u.exe"),
            mk("HKLM (32-bit)", "u.exe"),
            other,
        ]);
        assert_eq!(v.len(), 2);
        let one = v.iter().find(|a| a.version == "1.0").unwrap();
        assert_eq!(
            one.uninstall_string, "u.exe",
            "keeps the entry that can uninstall"
        );
    }

    #[test]
    fn nameless_keys_skipped() {
        assert!(parse_entry("x", &RawApp::default(), "HKLM").is_none());
    }

    #[test]
    fn bad_dates_blank() {
        assert_eq!(format_install_date("2024-03"), "");
        assert_eq!(format_install_date(""), "");
    }

    #[test]
    fn winget_table() {
        let t = "Name            Id                Version  Source\n\
                 --------------------------------------------------\n\
                 Git             Git.Git           2.55.0   winget\n\
                 Some App        Vendor.SomeApp    1.0      winget\n";
        let a = parse_winget_list(t);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].id, "Git.Git");
        assert_eq!(a[1].name, "Some App");
        assert_eq!(a[1].version, "1.0");
    }

    #[test]
    fn winget_id_validation() {
        assert!(winget_uninstall("bad id").is_err());
        assert!(winget_uninstall("").is_err());
    }

    const UPGRADE_SAMPLE: &str = "\r   - \r   \\ \r   | \r                              \r\n\
Name                         Id                          Version   Available  Source\n\
------------------------------------------------------------------------------------\n\
Git                          Git.Git                     2.45.0    2.46.0     winget\n\
Microsoft Visual Studio Code Microsoft.VisualStudioCode  1.90.0    1.91.1     winget\n\
NVIDIA Graphics Driver 551   Nvidia.GraphicsDriver       551.23    552.44     winget\n\
AMD Software: Adrenalin Edit AMD.AMDSoftwareAdrenalin    23.1.1    24.1.1     winget\n\
Intel(R) Graphics Driver     Intel.IntelGraphicsDriver   31.0.101  31.0.102   winget\n\
7-Zip                        7zip.7zip                   Unknown   24.07      winget\n\
Some Store App               9NBLGGH4NNS1                1.0       1.1        msstore\n\
7 upgrades available.\n\
\n\
The following packages have an upgrade available, but require explicit targeting for upgrade:\n\
Name           Id                  Version  Available  Source\n\
------------------------------------------------------------\n\
Pinned Tool    Vendor.PinnedTool   1.0      2.0        winget\n\
1 package(s) have pins that prevent upgrade.\n";

    #[test]
    fn parses_winget_upgrade_table() {
        let v = parse_winget_upgrade(UPGRADE_SAMPLE);
        let ids: Vec<&str> = v.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "Git.Git",
                "Microsoft.VisualStudioCode",
                "Nvidia.GraphicsDriver",
                "AMD.AMDSoftwareAdrenalin",
                "Intel.IntelGraphicsDriver",
                "7zip.7zip",
                "9NBLGGH4NNS1",
                "Vendor.PinnedTool"
            ]
        );
        assert_eq!(v[0].name, "Git");
        assert_eq!(v[0].version, "2.45.0");
        assert_eq!(v[0].available, "2.46.0");
        assert_eq!(v[0].source, "winget");
        assert_eq!(v[1].name, "Microsoft Visual Studio Code");
        assert_eq!(v[5].version, "Unknown");
        assert_eq!(v[6].source, "msstore");
        assert_eq!(v[7].name, "Pinned Tool");
    }

    #[test]
    fn upgrade_parser_ignores_noise_and_empty_results() {
        assert!(parse_winget_upgrade("").is_empty());
        assert!(
            parse_winget_upgrade("No installed package found matching input criteria.\n")
                .is_empty()
        );
        assert!(parse_winget_upgrade("random\ntext\n").is_empty());
        // A table with no rows.
        let t = "Name  Id  Version  Available  Source\n---------------------------------\n0 upgrades available.\n";
        assert!(parse_winget_upgrade(t).is_empty());
        // Rows without an "Available" value are not upgrades.
        let t = "Name   Id       Version  Available  Source\n---------------------------------------\nThing  A.Thing  1.0                 winget\n";
        assert!(parse_winget_upgrade(t).is_empty());
    }

    #[test]
    fn upgrade_parser_without_source_column() {
        let t = "Name   Id       Version  Available\n------------------------------------\nThing  A.Thing  1.0      1.1\n";
        let v = parse_winget_upgrade(t);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].available, "1.1");
        assert_eq!(v[0].source, "");
    }

    #[test]
    fn gpu_driver_packages_are_excluded() {
        let l = split_upgrades(parse_winget_upgrade(UPGRADE_SAMPLE));
        let kept: Vec<&str> = l.items.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(
            kept,
            vec![
                "Git.Git",
                "Microsoft.VisualStudioCode",
                "7zip.7zip",
                "9NBLGGH4NNS1",
                "Vendor.PinnedTool"
            ]
        );
        let out: Vec<&str> = l.excluded.iter().map(|(u, _)| u.id.as_str()).collect();
        assert_eq!(
            out,
            vec![
                "Nvidia.GraphicsDriver",
                "AMD.AMDSoftwareAdrenalin",
                "Intel.IntelGraphicsDriver"
            ]
        );
        assert!(l.excluded[0].1.contains("NVIDIA"));
        assert!(l.excluded[1].1.contains("AMD"));
        assert!(l.excluded[2].1.contains("Intel"));
    }

    #[test]
    fn gpu_rules_match_ids_and_names_but_not_unrelated_apps() {
        for (n, i) in [
            ("NVIDIA GeForce Experience", "Nvidia.GeForceExperience"),
            ("NVIDIA app", "Nvidia.NvidiaApp"),
            ("", "NVIDIA.GameReadyDriver"),
            ("AMD Software: Adrenalin Edition", ""),
            ("Radeon Software", "AMD.RadeonSoftware"),
            ("Intel Arc Control", "Intel.ArcControl"),
            (
                "Intel Driver & Support Assistant",
                "Intel.IntelDriverAndSupportAssistant",
            ),
            ("Intel UHD Graphics Driver", ""),
        ] {
            assert!(gpu_driver_reason(n, i).is_some(), "{n} / {i}");
        }
        for (n, i) in [
            ("Git", "Git.Git"),
            ("Intel Unite", "Intel.Unite"),
            ("AMD Chipset Software", "AMD.Chipset"),
            ("NVIDIA Broadcast Tool", "Nvidia.Broadcast"),
            ("Microsoft Edge", "Microsoft.Edge"),
            ("Radiant Player", "Radiant.Player"),
        ] {
            assert!(gpu_driver_reason(n, i).is_none(), "{n} / {i}");
        }
    }

    #[test]
    fn winget_ids_are_validated() {
        for ok in [
            "Git.Git",
            "7zip.7zip",
            "9NBLGGH4NNS1",
            "Vendor.App-Name_2+",
            r"ARP\User\X64\{GUID}",
        ] {
            assert!(validate_winget_id(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "bad id", "a\"b", "-rf", "--force", "a&b", "a|b", "a>b", "a%PATH%", "a\tb", "a\nb",
        ] {
            assert!(validate_winget_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn upgrade_refuses_bad_and_gpu_ids_before_running_anything() {
        assert!(matches!(
            winget_upgrade("bad id"),
            Err(CoreError::Blocked(_))
        ));
        assert!(matches!(winget_upgrade(""), Err(CoreError::Blocked(_))));
        assert!(matches!(
            winget_upgrade("--silent"),
            Err(CoreError::Blocked(_))
        ));
        assert!(matches!(
            winget_upgrade("Nvidia.GraphicsDriver"),
            Err(CoreError::Blocked(_))
        ));
    }

    #[test]
    fn parses_real_winget_layout_with_adjacent_columns() {
        // Layout captured from `winget upgrade` on Windows 11 (names changed): the version
        // columns touch with a single space, and one version is "Unknown".
        let t = "Name                                            Id                                    Version       Available     Source\r\n\
------------------------------------------------------------------------------------------------------------------------\r\n\
Microsoft Edge                                  Microsoft.Edge                        155.0.4283.45 155.0.4283.62 winget\r\n\
Game Player for alice                           Vendor.Player                         Unknown       0.743         winget\r\n\
Visual Studio Community 2022                    Microsoft.VisualStudio.2022.Community 17.14.41      17.14.42      winget\r\n\
10 upgrades available.\r\n";
        let v = parse_winget_upgrade(t);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].version, "155.0.4283.45");
        assert_eq!(v[0].available, "155.0.4283.62");
        assert_eq!(v[1].version, "Unknown");
        assert_eq!(v[2].id, "Microsoft.VisualStudio.2022.Community");
        assert_eq!(v[2].available, "17.14.42");
        assert!(v.iter().all(|u| u.source == "winget"));
    }

    #[test]
    fn truncated_ids_are_not_usable() {
        assert!(validate_winget_id("Microsoft.VisualStudio.2022.Comm\u{2026}").is_err());
    }
}
