//! Installed-application listing and uninstall launching.

use crate::error::{CoreError, Result};
use crate::util::{run_hidden, shell_execute, split_command_line};
use std::collections::HashMap;
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
    let get = |k: &str| raw.strings.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
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
        let Ok(k) = key.open_subkey(&sub) else { continue };
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
    read_hive(RegKey::predef(HKEY_LOCAL_MACHINE), P32, "HKLM (32-bit)", &mut out);
    read_hive(RegKey::predef(HKEY_CURRENT_USER), P, "HKCU", &mut out);
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Launch the uninstaller. ShellExecute lets the installer request UAC itself.
pub fn uninstall(app: &AppEntry, quiet: bool) -> Result<()> {
    let cmd = if quiet && !app.quiet_uninstall_string.is_empty() {
        &app.quiet_uninstall_string
    } else {
        &app.uninstall_string
    };
    if cmd.trim().is_empty() {
        return Err(CoreError::Message(format!("'{}' has no uninstall command.", app.name)));
    }
    let (prog, args) = split_command_line(cmd);
    info!("launching uninstaller for '{}': {cmd}", app.name);
    shell_execute("open", &prog, if args.is_empty() { None } else { Some(&args) })
}

// ---------- winget (optional) ----------

#[derive(Debug, Clone)]
pub struct WingetApp {
    pub name: String,
    pub id: String,
    pub version: String,
}

pub fn winget_available() -> bool {
    run_hidden("winget", &["--version"]).map(|(c, _)| c == 0).unwrap_or(false)
}

pub fn parse_winget_list(text: &str) -> Vec<WingetApp> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(h) = lines.iter().position(|l| l.trim_start().starts_with("Name") && l.contains("Id")) else {
        return vec![];
    };
    let header: Vec<char> = lines[h].chars().collect();
    let col = |label: &str| lines[h].find(label).map(|b| lines[h][..b].chars().count());
    let (Some(id_c), Some(ver_c)) = (col("Id"), col("Version")) else {
        return vec![];
    };
    let end_ver = col("Available").or_else(|| col("Source")).unwrap_or(header.len().max(ver_c + 1));
    let slice = |l: &str, a: usize, b: usize| -> String {
        l.chars().skip(a).take(b.saturating_sub(a)).collect::<String>().trim().to_string()
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
        &["list", "--disable-interactivity", "--accept-source-agreements"],
    )?;
    if code != 0 {
        return Err(CoreError::Message("winget list failed.".into()));
    }
    Ok(parse_winget_list(&out))
}

pub fn winget_uninstall(id: &str) -> Result<()> {
    if id.is_empty() || id.chars().any(|c| c.is_whitespace() || c == '"') {
        return Err(CoreError::Blocked("Invalid winget package id.".into()));
    }
    info!("winget uninstall {id}");
    shell_execute("open", "winget", Some(&format!("uninstall --id {id} --exact")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(strings: &[(&str, &str)], dwords: &[(&str, u32)]) -> RawApp {
        RawApp {
            strings: strings.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
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
        let u = raw(&[("DisplayName", "Update for X"), ("ParentKeyName", "OperatingSystem")], &[]);
        assert!(parse_entry("KB123", &u, "HKLM").unwrap().is_update);
        let s = raw(&[("DisplayName", "Runtime")], &[("SystemComponent", 1)]);
        assert!(parse_entry("rt", &s, "HKLM").unwrap().is_system_component);
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
}
