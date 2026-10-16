//! Export third-party drivers with `pnputil /export-driver * <folder>` (admin).

use crate::admin::require_admin;
use crate::error::{CoreError, Result};
use std::io::{BufRead, BufReader};
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

pub fn backup_drivers(dest: &Path, mut progress: impl FnMut(&str)) -> Result<BackupResult> {
    require_admin()?;
    validate_destination(dest)?;
    std::fs::create_dir_all(dest)?;
    info!("exporting drivers to {}", dest.display());
    let mut child = Command::new("pnputil")
        .arg("/export-driver")
        .arg("*")
        .arg(dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()?;
    let mut res = BackupResult::default();
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if is_export_line(&line) {
                res.exported += 1;
            }
            progress(&line);
            res.log.push(line);
        }
    }
    let status = child.wait()?;
    res.exit_code = status.code().unwrap_or(-1);
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
    fn export_line_detection() {
        assert!(is_export_line("Driver package exported successfully."));
        assert!(!is_export_line("Exporting driver package: oem1.inf"));
    }
}
