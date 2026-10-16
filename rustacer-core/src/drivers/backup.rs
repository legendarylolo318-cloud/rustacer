//! Export third-party drivers with `pnputil /export-driver * <folder>` (admin).

use crate::admin::require_admin;
use crate::error::{CoreError, Result};
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::info;

#[derive(Debug, Clone, Default)]
pub struct BackupResult {
    pub exported: usize,
    pub exit_code: i32,
    pub log: Vec<String>,
}

/// Reject drive roots and non-directories.
pub fn validate_destination(dest: &Path) -> Result<()> {
    if dest.parent().is_none() {
        return Err(CoreError::Blocked(
            "Choose a folder, not a drive root.".into(),
        ));
    }
    if dest.exists() && !dest.is_dir() {
        return Err(CoreError::Message(
            "The destination is not a folder.".into(),
        ));
    }
    Ok(())
}

/// Count lines in pnputil output that report an exported package.
pub fn is_export_line(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("exported") && l.contains("successfully")
}

/// Parse the trailing "Exported driver packages: N" summary, if present.
pub fn parse_summary(line: &str) -> Option<usize> {
    let l = line.to_ascii_lowercase();
    if !l.starts_with("exported driver packages") {
        return None;
    }
    l.rsplit(':').next()?.trim().parse().ok()
}

/// Split raw console bytes into trimmed, non-empty lines (lossy: pnputil writes
/// in the OEM code page, which is not always valid UTF-8).
pub fn split_lines(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == b'\n')
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn backup_drivers(dest: &Path, mut progress: impl FnMut(&str)) -> Result<BackupResult> {
    require_admin()?;
    validate_destination(dest)?;
    std::fs::create_dir_all(dest)?;
    info!("exporting drivers to {}", dest.display());
    // The path is passed as its own argument, so Rust quotes paths with spaces correctly.
    let mut child = Command::new("pnputil")
        .arg("/export-driver")
        .arg("*")
        .arg(dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()?;
    // Drain stderr on its own thread so neither pipe can fill up and block pnputil.
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_end(&mut buf);
        }
        buf
    });
    let mut res = BackupResult::default();
    let mut summary = None;
    if let Some(out) = child.stdout.take() {
        let mut reader = BufReader::new(out);
        let mut raw = Vec::new();
        while reader.read_until(b'\n', &mut raw).unwrap_or(0) > 0 {
            for line in split_lines(&raw) {
                if is_export_line(&line) {
                    res.exported += 1;
                }
                if let Some(n) = parse_summary(&line) {
                    summary = Some(n);
                }
                progress(&line);
                res.log.push(line);
            }
            raw.clear();
        }
    }
    let status = child.wait()?;
    for line in split_lines(&err_thread.join().unwrap_or_default()) {
        progress(&line);
        res.log.push(line);
    }
    if let Some(n) = summary {
        res.exported = n;
    }
    res.exit_code = status.code().unwrap_or(-1);
    info!("pnputil exited {} ({} packages)", res.exit_code, res.exported);
    if res.exit_code != 0 && res.exported == 0 {
        return Err(CoreError::Message(format!(
            "pnputil failed with exit code {}.",
            res.exit_code
        )));
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_rejected() {
        assert!(validate_destination(Path::new(r"C:\")).is_err());
        assert!(validate_destination(Path::new(r"C:\Backups\drivers")).is_ok());
    }

    #[test]
    fn summary_and_lossy_lines() {
        assert_eq!(parse_summary("Exported driver packages:   37"), Some(37));
        assert_eq!(parse_summary("Total driver packages: 40"), None);
        let raw = b"Exporting driver package: oem1.inf\r\nDriver package exported successfully.\r\n\xff\xfe bad bytes\r\n\r\n";
        let l = split_lines(raw);
        assert_eq!(l.len(), 3);
        assert!(is_export_line(&l[1]));
    }

    #[test]
    fn export_line_detection() {
        assert!(is_export_line("Driver package exported successfully."));
        assert!(!is_export_line("Exporting driver package: oem1.inf"));
    }
}

