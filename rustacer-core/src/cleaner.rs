//! System cleaner. Hard rule: nothing outside a target's whitelisted roots
//! is ever deleted. Every candidate is canonicalised and re-checked, and
//! symlinks / junctions (reparse points) are never followed or removed.

use crate::admin::is_elevated;
use crate::error::{CoreError, Result};
use crate::util::wide;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    UserTemp,
    WindowsTemp,
    RecycleBin,
    ThumbnailCache,
    ErrorReports,
    WindowsUpdateCache,
    ChromeCache,
    EdgeCache,
    FirefoxCache,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::UserTemp => "User temporary files",
            Category::WindowsTemp => "Windows temporary files",
            Category::RecycleBin => "Recycle Bin",
            Category::ThumbnailCache => "Thumbnail cache",
            Category::ErrorReports => "Windows Error Reporting dumps",
            Category::WindowsUpdateCache => "Windows Update download cache",
            Category::ChromeCache => "Google Chrome cache",
            Category::EdgeCache => "Microsoft Edge cache",
            Category::FirefoxCache => "Mozilla Firefox cache",
        }
    }
}

/// One cleanable location set.
#[derive(Debug, Clone)]
pub struct Target {
    pub category: Category,
    /// Whitelisted root directories. Only their contents may be deleted.
    pub roots: Vec<PathBuf>,
    /// If set, only files whose lowercase name starts with one of these are touched.
    pub file_prefixes: Option<Vec<String>>,
    pub needs_admin: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SkippedItem {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub category: Category,
    pub bytes: u64,
    pub files: u64,
    pub skipped: Vec<SkippedItem>,
    /// Set when the category cannot be cleaned right now (e.g. needs admin).
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CleanReport {
    pub freed_bytes: u64,
    pub files_deleted: u64,
    pub skipped: Vec<SkippedItem>,
}

// ---------- path safety ----------

fn is_reparse(meta: &std::fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Roots must be absolute, must not themselves be symlinks/junctions, and must not be
/// too shallow (drive root, C:\Windows, etc.).
pub fn validate_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return Err(CoreError::Blocked(format!(
            "'{}' is not an absolute path",
            root.display()
        )));
    }
    let meta = std::fs::symlink_metadata(root)
        .map_err(|e| CoreError::Blocked(format!("{}: {e}", root.display())))?;
    if is_reparse(&meta) || !meta.is_dir() {
        return Err(CoreError::Blocked(format!(
            "'{}' is a link or not a folder; refusing to clean it",
            root.display()
        )));
    }
    let canon = root
        .canonicalize()
        .map_err(|e| CoreError::Blocked(format!("{}: {e}", root.display())))?;
    let depth = canon
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    if depth < 2 {
        return Err(CoreError::Blocked(format!(
            "'{}' is too close to a drive root to be cleaned",
            canon.display()
        )));
    }
    Ok(canon)
}

/// True if `candidate` (after canonicalisation) lies strictly inside `root_canon`.
/// `root_canon` must already be canonical.
pub fn is_inside(root_canon: &Path, candidate: &Path) -> bool {
    match candidate.canonicalize() {
        Ok(c) => c != root_canon && c.starts_with(root_canon),
        Err(_) => false,
    }
}

fn name_matches(prefixes: &Option<Vec<String>>, name: &str) -> bool {
    match prefixes {
        None => true,
        Some(p) => {
            let l = name.to_ascii_lowercase();
            p.iter().any(|x| l.starts_with(x))
        }
    }
}

// ---------- walking ----------

struct Walk<'a> {
    root: &'a Path,
    prefixes: &'a Option<Vec<String>>,
    cancel: &'a AtomicBool,
}

impl Walk<'_> {
    /// Recursively visit files; `on_file(path, size)` returns whether it was deleted.
    fn visit(
        &self,
        dir: &Path,
        skipped: &mut Vec<SkippedItem>,
        on_file: &mut dyn FnMut(&Path, u64, &mut Vec<SkippedItem>),
        remove_empty_dirs: bool,
    ) {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) => {
                skipped.push(SkippedItem {
                    path: dir.to_path_buf(),
                    reason: e.to_string(),
                });
                return;
            }
        };
        for entry in rd.flatten() {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    skipped.push(SkippedItem {
                        path,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            if is_reparse(&meta) {
                skipped.push(SkippedItem {
                    path,
                    reason: "symlink/junction left untouched".into(),
                });
                continue;
            }
            if meta.is_dir() {
                self.visit(&path, skipped, on_file, remove_empty_dirs);
                if remove_empty_dirs && is_inside(self.root, &path) {
                    // Fails harmlessly if not empty.
                    let _ = std::fs::remove_dir(&path);
                }
            } else if name_matches(self.prefixes, &entry.file_name().to_string_lossy()) {
                on_file(&path, meta.len(), skipped);
            }
        }
    }
}

// ---------- targets ----------

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

fn existing(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| p.is_dir()).collect()
}

/// Keep only absolute paths whose final component is one of `leaves` (case-insensitive).
/// This stops a hostile or odd environment variable (e.g. %TEMP%=C:\Users\me) from
/// turning an unrelated folder into a cleaning root.
pub fn sane_roots(paths: Vec<PathBuf>, leaves: &[&str]) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| p.is_absolute() && leaf_ok(p, leaves))
        .collect()
}

fn leaf_ok(p: &Path, leaves: &[&str]) -> bool {
    let name = |q: &Path| {
        q.file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    };
    let leaf = name(p);
    if leaves.contains(&leaf.as_str()) {
        return true;
    }
    // Per-session temp folders such as ...\Temp\2 (Remote Desktop).
    leaves.contains(&"temp")
        && !leaf.is_empty()
        && leaf.chars().all(|c| c.is_ascii_digit())
        && p.parent().is_some_and(|parent| name(parent) == "temp")
}

/// Subdirectories of a Chromium "User Data" dir that hold only cache data.
fn chromium_cache_dirs(user_data: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(user_data) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "Default" || name.starts_with("Profile ") {
            for sub in ["Cache", "Code Cache", "GPUCache"] {
                out.push(e.path().join(sub));
            }
        }
    }
    existing(out)
}

fn firefox_cache_dirs(local: &Path) -> Vec<PathBuf> {
    let profiles = local.join("Mozilla").join("Firefox").join("Profiles");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(profiles) {
        for e in rd.flatten() {
            out.push(e.path().join("cache2"));
        }
    }
    existing(out)
}

/// The real, whitelisted system targets.
pub fn system_targets() -> Vec<Target> {
    // Unset variables yield empty paths, which `sane_roots` rejects (not absolute).
    let local = env_path("LOCALAPPDATA").unwrap_or_default();
    let windir = env_path("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let progdata = env_path("ProgramData").unwrap_or_default();
    let temp = env_path("TEMP").unwrap_or_else(|| local.join("Temp"));
    let temp_leaves: &[&str] = &["temp", "tmp"];
    let cache_leaves: &[&str] = &["cache", "code cache", "gpucache", "cache2"];

    vec![
        Target {
            category: Category::UserTemp,
            roots: sane_roots(existing(vec![temp]), temp_leaves),
            file_prefixes: None,
            needs_admin: false,
        },
        Target {
            category: Category::WindowsTemp,
            roots: sane_roots(existing(vec![windir.join("Temp")]), &["temp"]),
            file_prefixes: None,
            needs_admin: true,
        },
        Target {
            category: Category::RecycleBin,
            roots: vec![],
            file_prefixes: None,
            needs_admin: false,
        },
        Target {
            category: Category::ThumbnailCache,
            roots: sane_roots(
                existing(vec![local
                    .join("Microsoft")
                    .join("Windows")
                    .join("Explorer")]),
                &["explorer"],
            ),
            file_prefixes: Some(vec!["thumbcache_".into(), "iconcache_".into()]),
            needs_admin: false,
        },
        Target {
            category: Category::ErrorReports,
            roots: sane_roots(
                existing(vec![
                    local.join("Microsoft").join("Windows").join("WER"),
                    progdata
                        .join("Microsoft")
                        .join("Windows")
                        .join("WER")
                        .join("ReportArchive"),
                    progdata
                        .join("Microsoft")
                        .join("Windows")
                        .join("WER")
                        .join("ReportQueue"),
                ]),
                &["wer", "reportarchive", "reportqueue"],
            ),
            file_prefixes: None,
            needs_admin: false,
        },
        Target {
            category: Category::WindowsUpdateCache,
            roots: sane_roots(
                existing(vec![windir.join("SoftwareDistribution").join("Download")]),
                &["download"],
            ),
            file_prefixes: None,
            needs_admin: true,
        },
        Target {
            category: Category::ChromeCache,
            roots: sane_roots(
                chromium_cache_dirs(&local.join("Google").join("Chrome").join("User Data")),
                cache_leaves,
            ),
            file_prefixes: None,
            needs_admin: false,
        },
        Target {
            category: Category::EdgeCache,
            roots: sane_roots(
                chromium_cache_dirs(&local.join("Microsoft").join("Edge").join("User Data")),
                cache_leaves,
            ),
            file_prefixes: None,
            needs_admin: false,
        },
        Target {
            category: Category::FirefoxCache,
            roots: sane_roots(firefox_cache_dirs(&local), cache_leaves),
            file_prefixes: None,
            needs_admin: false,
        },
    ]
}

// ---------- scan ----------

pub fn scan_target(t: &Target, cancel: &AtomicBool) -> ScanResult {
    let mut res = ScanResult {
        category: t.category,
        bytes: 0,
        files: 0,
        skipped: vec![],
        unavailable: None,
    };
    if t.needs_admin && !is_elevated() {
        res.unavailable = Some("Requires administrator rights".into());
    }
    if t.category == Category::RecycleBin {
        match recycle_bin_size() {
            Ok((b, n)) => {
                res.bytes = b;
                res.files = n;
            }
            Err(e) => res.unavailable = Some(e.to_string()),
        }
        return res;
    }
    for root in &t.roots {
        let Ok(canon) = validate_root(root) else {
            continue;
        };
        let walk = Walk {
            root: &canon,
            prefixes: &t.file_prefixes,
            cancel,
        };
        let mut skipped = Vec::new();
        let (mut bytes, mut files) = (0u64, 0u64);
        walk.visit(
            &canon,
            &mut skipped,
            &mut |_, size, _| {
                bytes += size;
                files += 1;
            },
            false,
        );
        res.bytes += bytes;
        res.files += files;
        res.skipped.extend(skipped);
    }
    res
}

// ---------- clean ----------

pub fn clean_target(t: &Target, cancel: &AtomicBool) -> Result<CleanReport> {
    if t.needs_admin && !is_elevated() {
        return Err(CoreError::NeedsAdmin);
    }
    if t.category == Category::RecycleBin {
        let (before, n) = recycle_bin_size().unwrap_or((0, 0));
        empty_recycle_bin()?;
        info!("emptied recycle bin ({before} bytes)");
        return Ok(CleanReport {
            freed_bytes: before,
            files_deleted: n,
            skipped: vec![],
        });
    }

    // Windows Update cache: stop wuauserv (and restart only if it was running).
    let mut restart_wu = false;
    if t.category == Category::WindowsUpdateCache
        && crate::services::query_state("wuauserv").unwrap_or_default()
            == crate::services::ServiceState::Running
    {
        crate::services::stop_and_wait("wuauserv", 30)?;
        restart_wu = true;
    }
    let report = clean_roots(&t.roots, &t.file_prefixes, cancel);
    if restart_wu {
        if let Err(e) = crate::services::start_service("wuauserv") {
            warn!("could not restart wuauserv: {e}");
        }
    }
    info!(
        "cleaned {:?}: {} files, {} bytes, {} skipped",
        t.category,
        report.files_deleted,
        report.freed_bytes,
        report.skipped.len()
    );
    Ok(report)
}

/// Delete the contents of whitelisted roots. Exposed for tests on temp dirs.
pub fn clean_roots(
    roots: &[PathBuf],
    prefixes: &Option<Vec<String>>,
    cancel: &AtomicBool,
) -> CleanReport {
    let mut report = CleanReport::default();
    for root in roots {
        let canon = match validate_root(root) {
            Ok(c) => c,
            Err(e) => {
                report.skipped.push(SkippedItem {
                    path: root.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };
        let walk = Walk {
            root: &canon,
            prefixes,
            cancel,
        };
        let mut skipped = Vec::new();
        let mut freed = 0u64;
        let mut deleted = 0u64;
        walk.visit(
            &canon,
            &mut skipped,
            &mut |path, size, skipped| {
                // Re-verify containment right before deleting.
                if !is_inside(&canon, path) {
                    skipped.push(SkippedItem {
                        path: path.to_path_buf(),
                        reason: "resolves outside the whitelisted folder".into(),
                    });
                    return;
                }
                let mut r = std::fs::remove_file(path);
                if r.is_err() {
                    if let Ok(m) = std::fs::metadata(path) {
                        let mut p = m.permissions();
                        if p.readonly() {
                            #[allow(clippy::permissions_set_readonly_false)]
                            p.set_readonly(false);
                            let _ = std::fs::set_permissions(path, p);
                            r = std::fs::remove_file(path);
                        }
                    }
                }
                match r {
                    Ok(()) => {
                        freed += size;
                        deleted += 1;
                    }
                    Err(e) => skipped.push(SkippedItem {
                        path: path.to_path_buf(),
                        reason: format!("in use or protected ({e})"),
                    }),
                }
            },
            true,
        );
        report.freed_bytes += freed;
        report.files_deleted += deleted;
        report.skipped.extend(skipped);
    }
    report
}

// ---------- recycle bin ----------

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHEmptyRecycleBinW, SHQueryRecycleBinW, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI,
    SHERB_NOSOUND, SHQUERYRBINFO,
};

pub fn recycle_bin_size() -> Result<(u64, u64)> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: info is a valid, correctly sized struct; null root = all drives.
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info)? };
    Ok((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

pub fn empty_recycle_bin() -> Result<()> {
    let _ = wide("");
    // SAFETY: null window and root are permitted by the API.
    unsafe {
        SHEmptyRecycleBinW(
            None,
            PCWSTR::null(),
            SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
        )?
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn refuses_drive_root() {
        assert!(validate_root(Path::new(r"C:\")).is_err());
    }

    #[test]
    fn inside_check() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("cache");
        fs::create_dir(&root).unwrap();
        let f = root.join("a.txt");
        fs::write(&f, "x").unwrap();
        let outside = d.path().join("other.txt");
        fs::write(&outside, "x").unwrap();
        let rc = root.canonicalize().unwrap();
        assert!(is_inside(&rc, &f));
        assert!(!is_inside(&rc, &outside));
        assert!(!is_inside(&rc, &rc), "the root itself is never a candidate");
        // dot-dot escape
        assert!(!is_inside(&rc, &root.join("..").join("other.txt")));
        // nonexistent
        assert!(!is_inside(&rc, &root.join("nope")));
    }

    #[test]
    fn scan_counts_sizes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a"), vec![0u8; 100]).unwrap();
        fs::write(root.join("sub").join("b"), vec![0u8; 50]).unwrap();
        let t = Target {
            category: Category::UserTemp,
            roots: vec![root],
            file_prefixes: None,
            needs_admin: false,
        };
        let r = scan_target(&t, &no_cancel());
        assert_eq!(r.bytes, 150);
        assert_eq!(r.files, 2);
    }

    #[test]
    fn clean_deletes_contents_only() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a"), "123").unwrap();
        fs::write(root.join("sub").join("b"), "45").unwrap();
        let keep = d.path().join("keep.txt");
        fs::write(&keep, "keep").unwrap();
        let rep = clean_roots(std::slice::from_ref(&root), &None, &no_cancel());
        assert_eq!(rep.files_deleted, 2);
        assert_eq!(rep.freed_bytes, 5);
        assert!(root.exists(), "root dir itself is preserved");
        assert!(!root.join("sub").exists(), "empty subdir pruned");
        assert!(keep.exists());
    }

    #[test]
    fn prefix_filter_limits_deletion() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("explorer");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("thumbcache_32.db"), "x").unwrap();
        fs::write(root.join("important.dat"), "x").unwrap();
        let rep = clean_roots(
            std::slice::from_ref(&root),
            &Some(vec!["thumbcache_".into()]),
            &no_cancel(),
        );
        assert_eq!(rep.files_deleted, 1);
        assert!(root.join("important.dat").exists());
    }

    #[test]
    fn junction_pointing_outside_is_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        let outside = d.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious.txt"), "data").unwrap();
        // Create a junction via mklink /J (no admin needed).
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(&outside)
            .output()
            .unwrap();
        if !status.status.success() {
            return; // environment can't make junctions; nothing to assert
        }
        let rep = clean_roots(std::slice::from_ref(&root), &None, &no_cancel());
        assert!(outside.join("precious.txt").exists());
        assert!(rep.skipped.iter().any(|s| s.reason.contains("symlink")));
    }

    #[test]
    fn relative_and_empty_roots_refused() {
        assert!(validate_root(Path::new("")).is_err());
        assert!(validate_root(Path::new("Temp")).is_err());
        assert!(validate_root(Path::new(r"C:\Windows")).is_err());
        let d = tempfile::tempdir().unwrap();
        let r = clean_roots(&[PathBuf::from("src")], &None, &no_cancel());
        assert_eq!(r.files_deleted, 0);
        assert!(!r.skipped.is_empty());
        drop(d);
    }

    #[test]
    fn file_as_root_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("cache");
        fs::write(&f, "x").unwrap();
        assert!(validate_root(&f).is_err());
        assert!(f.exists());
    }

    #[test]
    fn env_style_roots_need_expected_leaf() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("Users").join("me");
        let temp = home.join("AppData").join("Local").join("Temp");
        let rdp = temp.join("2");
        fs::create_dir_all(&rdp).unwrap();
        let ok = sane_roots(
            vec![temp.clone(), rdp.clone(), home.clone()],
            &["temp", "tmp"],
        );
        assert_eq!(
            ok,
            vec![temp, rdp],
            "a %TEMP% pointing at a profile dir is dropped"
        );
        assert!(sane_roots(vec![PathBuf::from("Temp")], &["temp"]).is_empty());
        assert!(sane_roots(vec![PathBuf::new()], &["temp"]).is_empty());
        let numeric_elsewhere = d.path().join("Projects").join("2");
        fs::create_dir_all(&numeric_elsewhere).unwrap();
        assert!(sane_roots(vec![numeric_elsewhere], &["temp"]).is_empty());
    }

    #[test]
    fn junction_root_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("Documents");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("thesis.docx"), "data").unwrap();
        let link = d.path().join("Cache");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        if !ok.status.success() {
            return;
        }
        let rep = clean_roots(std::slice::from_ref(&link), &None, &no_cancel());
        assert_eq!(rep.files_deleted, 0);
        assert!(outside.join("thesis.docx").exists());
    }

    #[test]
    fn file_symlink_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        fs::create_dir(&root).unwrap();
        let target = d.path().join("outside.txt");
        fs::write(&target, "keep").unwrap();
        // Needs Developer Mode or admin; skip silently if unavailable.
        if std::os::windows::fs::symlink_file(&target, root.join("link.txt")).is_err() {
            return;
        }
        let rep = clean_roots(std::slice::from_ref(&root), &None, &no_cancel());
        assert!(target.exists());
        assert_eq!(rep.files_deleted, 0);
    }

    #[test]
    fn dotdot_entries_cannot_escape() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        fs::create_dir_all(root.join("a")).unwrap();
        let keep = d.path().join("keep.txt");
        fs::write(&keep, "x").unwrap();
        // A root expressed with .. still canonicalises to itself; siblings stay safe.
        let weird = root.join("a").join("..");
        let rep = clean_roots(&[weird], &None, &no_cancel());
        assert_eq!(rep.files_deleted, 0);
        assert!(keep.exists());
        assert!(!is_inside(&root.canonicalize().unwrap(), &keep));
    }

    #[test]
    fn system_targets_are_all_absolute_and_leaf_checked() {
        for t in system_targets() {
            for r in &t.roots {
                assert!(r.is_absolute(), "{r:?}");
                let leaf = r
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_ascii_lowercase();
                assert!(
                    [
                        "temp",
                        "tmp",
                        "explorer",
                        "wer",
                        "reportarchive",
                        "reportqueue",
                        "download",
                        "cache",
                        "code cache",
                        "gpucache",
                        "cache2"
                    ]
                    .contains(&leaf.as_str())
                        || leaf.chars().all(|c| c.is_ascii_digit()),
                    "{r:?}"
                );
                let l = r.to_string_lossy().to_ascii_lowercase();
                for banned in [
                    "cookies",
                    "history",
                    "login data",
                    "places.sqlite",
                    "network",
                ] {
                    assert!(!l.ends_with(banned), "{r:?}");
                }
            }
        }
    }

    #[test]
    fn cancel_stops_work() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), "1").unwrap();
        let c = AtomicBool::new(true);
        let rep = clean_roots(std::slice::from_ref(&root), &None, &c);
        assert_eq!(rep.files_deleted, 0);
        assert!(root.join("a").exists());
    }
}
