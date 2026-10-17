//! Disk analyzer: find the largest folders and files under a drive or folder.
//!
//! Scanning is read-only. Reparse points (junctions, symlinks, OneDrive placeholders) are
//! never followed, so nothing is counted twice and the walk cannot leave the chosen folder.
//! The only action that changes anything is [`recycle`], which moves user-picked paths to
//! the Recycle Bin (never a permanent delete) after [`check_deletable`] has approved each.

use crate::cleaner::is_reparse;
use crate::error::{CoreError, Result};
use crate::util::{format_bytes, wide, ComGuard};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsString;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeNameForVolumeMountPointW};
use windows::Win32::UI::Shell::{
    SHFileOperationW, FOF_ALLOWUNDO, FOF_NOERRORUI, FO_DELETE, SHFILEOPSTRUCTW,
};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const DRIVE_FIXED: u32 = 3;

/// How many of the largest files are kept.
pub const TOP_FILES: usize = 100;
/// Folders smaller than this (and deeper than the first level) are folded into their parent
/// in the result, so a whole-drive scan stays small.
pub const KEEP_MIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub inaccessible: u64,
    pub current: String,
}

#[derive(Debug, Clone)]
pub struct FolderNode {
    /// Full path.
    pub path: PathBuf,
    /// Display name (the full path for the scan root).
    pub name: String,
    pub parent: Option<usize>,
    /// Total bytes in this folder and everything below it.
    pub size: u64,
    /// Total files in this folder and everything below it.
    pub files: u64,
    /// Kept sub-folders, largest first.
    pub children: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct DiskReport {
    pub root: PathBuf,
    /// `nodes[0]` is the scan root.
    pub nodes: Vec<FolderNode>,
    /// Largest files, biggest first (at most [`TOP_FILES`]).
    pub top_files: Vec<FileEntry>,
    pub total_bytes: u64,
    pub total_files: u64,
    pub total_folders: u64,
    /// Folders that could not be read (access denied, vanished, ...).
    pub inaccessible_dirs: u64,
    /// Reparse points that were skipped.
    pub skipped_links: u64,
    /// Small folders folded into their parents in `nodes`.
    pub folded_folders: u64,
    pub cancelled: bool,
}

struct Raw {
    name: OsString,
    parent: Option<usize>,
    bytes: u64,
    files: u64,
}

/// Scan `root`. `on_progress` is called a few times per second.
pub fn scan(
    root: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&ScanProgress),
) -> Result<DiskReport> {
    if !root.is_absolute() {
        return Err(CoreError::Blocked(format!(
            "'{}' is not an absolute path",
            root.display()
        )));
    }
    let meta = std::fs::symlink_metadata(root)
        .map_err(|e| CoreError::Message(format!("Cannot read '{}': {e}", root.display())))?;
    if is_reparse(&meta) || !meta.is_dir() {
        return Err(CoreError::Message(format!(
            "'{}' is not a plain folder (links are not followed).",
            root.display()
        )));
    }

    let mut raw = vec![Raw {
        name: root.as_os_str().to_owned(),
        parent: None,
        bytes: 0,
        files: 0,
    }];
    let mut stack: Vec<(usize, PathBuf)> = vec![(0, root.to_path_buf())];
    let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
    let mut prog = ScanProgress::default();
    let mut skipped_links = 0u64;
    let mut cancelled = false;
    let mut last_report = Instant::now();

    while let Some((idx, dir)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => {
                prog.inaccessible += 1;
                continue;
            }
        };
        for entry in rd {
            let Ok(entry) = entry else {
                prog.inaccessible += 1;
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                prog.inaccessible += 1;
                continue;
            };
            if is_reparse(&meta) {
                skipped_links += 1;
                continue;
            }
            if meta.is_dir() {
                raw.push(Raw {
                    name: entry.file_name(),
                    parent: Some(idx),
                    bytes: 0,
                    files: 0,
                });
                prog.folders += 1;
                stack.push((raw.len() - 1, entry.path()));
            } else {
                let len = meta.len();
                raw[idx].bytes += len;
                raw[idx].files += 1;
                prog.files += 1;
                prog.bytes += len;
                let qualifies = heap.len() < TOP_FILES
                    || heap.peek().is_some_and(|Reverse((min, _))| len > *min);
                if qualifies {
                    heap.push(Reverse((len, dir.join(entry.file_name()))));
                    if heap.len() > TOP_FILES {
                        heap.pop();
                    }
                }
            }
        }
        if last_report.elapsed() >= Duration::from_millis(150) {
            last_report = Instant::now();
            prog.current = dir.to_string_lossy().into_owned();
            on_progress(&prog);
        }
    }
    prog.current.clear();
    on_progress(&prog);

    let mut top_files: Vec<FileEntry> = heap
        .into_iter()
        .map(|Reverse((size, path))| FileEntry { path, size })
        .collect();
    top_files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));

    let mut report = build_report(root, raw);
    report.top_files = top_files;
    report.inaccessible_dirs = prog.inaccessible;
    report.skipped_links = skipped_links;
    report.cancelled = cancelled;
    info!(
        "disk scan of {}: {} files, {} bytes, {} unreadable folders{}",
        root.display(),
        report.total_files,
        report.total_bytes,
        report.inaccessible_dirs,
        if cancelled { " (cancelled)" } else { "" }
    );
    Ok(report)
}

/// Roll sizes up the tree and drop small deep folders. Children always have a higher index
/// than their parent, so one reverse pass accumulates and one forward pass prunes.
fn build_report(root: &Path, raw: Vec<Raw>) -> DiskReport {
    let n = raw.len();
    let mut size: Vec<u64> = raw.iter().map(|r| r.bytes).collect();
    let mut files: Vec<u64> = raw.iter().map(|r| r.files).collect();
    for i in (1..n).rev() {
        if let Some(p) = raw[i].parent {
            size[p] += size[i];
            files[p] += files[i];
        }
    }
    let mut depth = vec![0usize; n];
    for i in 1..n {
        depth[i] = raw[i].parent.map_or(0, |p| depth[p] + 1);
    }
    // A kept folder's parent is always kept: sizes only grow towards the root.
    let keep: Vec<bool> = (0..n)
        .map(|i| i == 0 || depth[i] <= 1 || size[i] >= KEEP_MIN_BYTES)
        .collect();

    let mut map = vec![usize::MAX; n];
    let mut nodes: Vec<FolderNode> = Vec::new();
    for i in 0..n {
        if !keep[i] {
            continue;
        }
        let parent = raw[i].parent.map(|p| map[p]);
        let (path, name) = match parent {
            None => (root.to_path_buf(), root.to_string_lossy().into_owned()),
            Some(p) => (
                nodes[p].path.join(&raw[i].name),
                raw[i].name.to_string_lossy().into_owned(),
            ),
        };
        map[i] = nodes.len();
        nodes.push(FolderNode {
            path,
            name,
            parent,
            size: size[i],
            files: files[i],
            children: vec![],
        });
        if let Some(p) = parent {
            let me = nodes.len() - 1;
            nodes[p].children.push(me);
        }
    }
    let sizes: Vec<u64> = nodes.iter().map(|n| n.size).collect();
    for node in &mut nodes {
        node.children.sort_by_key(|&c| Reverse(sizes[c]));
    }
    DiskReport {
        root: root.to_path_buf(),
        total_bytes: size[0],
        total_files: files[0],
        total_folders: n as u64 - 1,
        folded_folders: (n - nodes.len()) as u64,
        nodes,
        ..Default::default()
    }
}

// ---------- what may be recycled ----------

fn normalise(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.trim_end_matches('\\').to_ascii_lowercase()
}

/// Why `path` must never be removed from this tool, or `None` when its location is fine.
/// This is a pure check on the path text (plus the Windows/Program Files environment
/// variables), so it also works for paths that do not exist.
pub fn protected_reason(path: &Path) -> Option<String> {
    let system_drive = std::env::var("SystemDrive")
        .ok()
        .and_then(|d| d.chars().next())
        .map(|c| c.to_ascii_lowercase());
    if let Some(r) = protected_reason_for(path, system_drive) {
        return Some(r);
    }
    let me = normalise(path);
    for var in [
        "SystemRoot",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
    ] {
        if let Some(v) = std::env::var_os(var) {
            let v = normalise(Path::new(&v));
            if v.len() > 3 && (me == v || me.starts_with(&format!("{v}\\"))) {
                return Some(format!("it is inside a protected Windows folder ({var})"));
            }
        }
    }
    None
}

/// The text-only part of [`protected_reason`]; `system_drive` is the lowercase drive letter.
pub fn protected_reason_for(path: &Path, system_drive: Option<char>) -> Option<String> {
    let mut letter = None;
    let mut names: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                letter = p
                    .as_os_str()
                    .to_string_lossy()
                    .trim_start_matches(r"\\?\")
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_lowercase());
            }
            Component::Normal(n) => names.push(n.to_string_lossy().to_ascii_lowercase()),
            Component::RootDir => {}
            Component::CurDir | Component::ParentDir => {
                return Some("it contains '.' or '..' components".into());
            }
        }
    }
    if names.is_empty() {
        return Some("it is a drive root".into());
    }
    let top = names[0].as_str();
    if top == "windows" || top == "programdata" || top.starts_with("program files") {
        return Some(format!("it is under a protected Windows folder (\\{top})"));
    }
    if matches!(
        top,
        "$recycle.bin" | "system volume information" | "recovery" | "$windows.~bt" | "boot"
    ) {
        return Some(format!("\\{top} is a system folder"));
    }
    if names.len() == 1 && letter.is_some() && letter == system_drive {
        return Some("it is a top-level folder of the system drive".into());
    }
    if top == "users" && names.len() <= 2 {
        return Some("it is the Users folder or a user profile folder".into());
    }
    let leaf = names[names.len() - 1].as_str();
    if matches!(
        leaf,
        "pagefile.sys" | "hiberfil.sys" | "swapfile.sys" | "bootmgr" | "$mft"
    ) {
        return Some(format!("{leaf} is a system file"));
    }
    None
}

/// Names Windows would silently rewrite (trailing dots or spaces) or that address an
/// alternate data stream (`file:stream`) are refused, so what is checked is what is acted on.
fn odd_name_reason(p: &Path) -> Option<String> {
    let s = p.to_string_lossy();
    let body = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if body.char_indices().any(|(i, c)| c == ':' && i != 1) {
        return Some("it names an alternate data stream or device".into());
    }
    for c in p.components() {
        if let Component::Normal(n) = c {
            let n = n.to_string_lossy();
            if n.ends_with('.') || n.ends_with(' ') {
                return Some("a name ends with a dot or space, which Windows rewrites".into());
            }
        }
    }
    None
}

/// `\\?\C:\x` -> `C:\x`; refuses anything that is not a plain drive-letter path.
fn plain_drive_path(canon: &Path) -> Result<PathBuf> {
    let s = canon.to_string_lossy().replace('/', "\\");
    let plain = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    let b = plain.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        return Err(CoreError::Blocked(format!(
            "'{plain}' is not on a local drive letter; the Recycle Bin is not used there"
        )));
    }
    Ok(PathBuf::from(plain))
}

/// Recycle Bin settings that decide whether Windows would delete permanently instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinPolicy {
    /// Group policy "Do not move deleted files to the Recycle Bin".
    pub no_recycle_policy: bool,
    /// "Don't move files to the Recycle Bin. Remove files immediately when deleted."
    pub nuke_on_delete: bool,
    /// Maximum size of this drive's Recycle Bin, when known.
    pub max_bytes: Option<u64>,
}

/// Why moving `item` bytes (of `total` for this drive) would not land in the Recycle Bin.
pub fn bin_refusal(p: &BinPolicy, item: u64, total: u64) -> Option<String> {
    if p.no_recycle_policy {
        return Some(
            "a policy on this PC turns the Recycle Bin off, so it would be deleted permanently"
                .into(),
        );
    }
    if p.nuke_on_delete {
        return Some(
            "the Recycle Bin is turned off for this drive, so it would be deleted permanently"
                .into(),
        );
    }
    if let Some(max) = p.max_bytes {
        if item > max || total > max {
            return Some(format!(
                "it is bigger than this drive's Recycle Bin ({}), so Windows would offer to delete it permanently",
                format_bytes(max)
            ));
        }
    }
    None
}

fn dword(root: &RegKey, path: &str, name: &str) -> Option<u32> {
    root.open_subkey(path).ok()?.get_value::<u32, _>(name).ok()
}

/// Read the Recycle Bin settings for drive `letter`.
pub fn bin_policy(letter: char) -> BinPolicy {
    const POL: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    const BIN: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut p = BinPolicy {
        no_recycle_policy: dword(&hkcu, POL, "NoRecycleFiles") == Some(1)
            || dword(&hklm, POL, "NoRecycleFiles") == Some(1),
        ..Default::default()
    };
    // The per-volume key is named after the volume GUID.
    let mount = wide(format!("{letter}:\\"));
    let mut buf = [0u16; 64];
    // SAFETY: `mount` is NUL-terminated and `buf` is a writable buffer of the given length.
    let ok =
        unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR::from_raw(mount.as_ptr()), &mut buf) }
            .is_ok();
    if ok {
        let name = String::from_utf16_lossy(&buf);
        if let (Some(a), Some(b)) = (name.find('{'), name.find('}')) {
            let key = format!(r"{BIN}\Volume\{}", &name[a..=b]);
            p.nuke_on_delete = dword(&hkcu, &key, "NukeOnDelete") == Some(1);
            p.max_bytes = dword(&hkcu, &key, "MaxCapacity").map(|mb| u64::from(mb) * 1024 * 1024);
        }
    }
    // Older single-setting layout.
    if dword(&hkcu, BIN, "UseGlobalSettings") == Some(1)
        && dword(&hkcu, BIN, "NukeOnDelete") == Some(1)
    {
        p.nuke_on_delete = true;
    }
    p
}

/// Check that `target` may be recycled given the folder that was scanned. Returns the
/// canonical path. Nothing is touched.
pub fn check_deletable(scan_root: &Path, target: &Path) -> Result<PathBuf> {
    let blocked = |why: String| CoreError::Blocked(format!("'{}': {why}", target.display()));
    if !target.is_absolute() {
        return Err(blocked("it is not an absolute path".into()));
    }
    if let Some(why) = protected_reason(target) {
        return Err(blocked(format!("refused because {why}")));
    }
    if let Some(why) = odd_name_reason(target) {
        return Err(blocked(why));
    }
    // The scanned folder itself may have been swapped for a link since the scan.
    let root_meta = std::fs::symlink_metadata(scan_root)
        .map_err(|e| blocked(format!("the scanned folder cannot be read ({e})")))?;
    if is_reparse(&root_meta) || !root_meta.is_dir() {
        return Err(blocked(
            "the scanned folder is now a link or not a folder; scan again".into(),
        ));
    }
    let meta = std::fs::symlink_metadata(target)
        .map_err(|e| blocked(format!("cannot be read ({e}); it may already be gone")))?;
    if is_reparse(&meta) {
        return Err(blocked(
            "it is a link or junction; links are never touched".into(),
        ));
    }
    if meta.file_attributes() & FILE_ATTRIBUTE_SYSTEM != 0 {
        return Err(blocked("it is a system file or folder".into()));
    }
    // Still inside the folder that was scanned, and no link on the way there.
    let rel = target
        .strip_prefix(scan_root)
        .map_err(|_| blocked("it is not inside the folder that was scanned".into()))?;
    if rel.as_os_str().is_empty() {
        return Err(blocked("it is the scanned folder itself".into()));
    }
    let mut walk = scan_root.to_path_buf();
    for c in rel.components() {
        walk.push(c);
        let m = std::fs::symlink_metadata(&walk)
            .map_err(|e| blocked(format!("{} cannot be read ({e})", walk.display())))?;
        if is_reparse(&m) {
            return Err(blocked(format!(
                "{} is a link or junction on the way to it",
                walk.display()
            )));
        }
    }
    let root_c = scan_root
        .canonicalize()
        .map_err(|e| blocked(format!("scanned folder cannot be resolved ({e})")))?;
    let canon = target
        .canonicalize()
        .map_err(|e| blocked(format!("cannot be resolved ({e})")))?;
    if canon == root_c || !canon.starts_with(&root_c) {
        return Err(blocked(
            "it resolves to outside the folder that was scanned".into(),
        ));
    }
    if let Some(why) = protected_reason(&canon) {
        return Err(blocked(format!("refused because {why}")));
    }
    Ok(canon)
}

/// Move user-picked paths to the Recycle Bin. Every path is validated first and nothing
/// moves if any one is refused. Only local fixed drives are accepted, and the Recycle Bin
/// settings are checked first, so Windows never falls back to a permanent delete for lack of
/// a Recycle Bin, a "remove immediately" setting, or an item too big for the bin.
pub fn recycle(scan_root: &Path, paths: &[PathBuf]) -> Result<usize> {
    if paths.is_empty() {
        return Ok(0);
    }
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut per_drive: std::collections::HashMap<char, u64> = std::collections::HashMap::new();
    let mut sizes: Vec<(char, u64, String)> = Vec::new();
    for p in paths {
        let canon = check_deletable(scan_root, p)?;
        // Act on exactly the path that was checked.
        let plain = plain_drive_path(&canon)?;
        let s = plain.to_string_lossy().into_owned();
        let letter = s.chars().next().unwrap_or('C').to_ascii_uppercase();
        let root = wide(format!("{letter}:\\"));
        // SAFETY: `root` is a live NUL-terminated buffer for the duration of the call.
        let kind = unsafe { GetDriveTypeW(PCWSTR::from_raw(root.as_ptr())) };
        if kind != DRIVE_FIXED {
            return Err(CoreError::Blocked(format!(
                "'{s}' is not on a fixed local drive; there is no Recycle Bin to move it to"
            )));
        }
        let meta = std::fs::symlink_metadata(&plain)?;
        let size = if meta.is_dir() {
            // A folder must not carry links along: refuse rather than reason about them.
            let rep = scan(&plain, &AtomicBool::new(false), &mut |_| {})?;
            if rep.skipped_links > 0 {
                return Err(CoreError::Blocked(format!(
                    "'{s}' contains {} link(s) or junction(s); remove those first",
                    rep.skipped_links
                )));
            }
            if rep.inaccessible_dirs > 0 {
                return Err(CoreError::Blocked(format!(
                    "'{s}' has {} folder(s) Rustacer cannot read, so it cannot be checked",
                    rep.inaccessible_dirs
                )));
            }
            rep.total_bytes
        } else {
            meta.len()
        };
        *per_drive.entry(letter).or_insert(0) += size;
        sizes.push((letter, size, s));
        targets.push(plain);
    }
    for (letter, size, s) in &sizes {
        let total = per_drive.get(letter).copied().unwrap_or(*size);
        if let Some(why) = bin_refusal(&bin_policy(*letter), *size, total) {
            return Err(CoreError::Blocked(format!("'{s}': {why}")));
        }
    }
    let paths = &targets;
    // pFrom is a list of NUL-terminated paths ended by an extra NUL.
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        let mut w = wide(p);
        w.pop();
        from.extend(w);
        from.push(0);
    }
    from.push(0);
    let _com = ComGuard::sta();
    // FOF_ALLOWUNDO sends to the Recycle Bin. FOF_NOCONFIRMATION is deliberately not set:
    // for an item too big for the bin Windows then asks, instead of deleting permanently.
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR::from_raw(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    // SAFETY: `op` is fully initialised and `from` outlives the call; with FOF_ALLOWUNDO the
    // operation moves items to the Recycle Bin.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(CoreError::Windows(format!(
            "Could not move the items to the Recycle Bin (shell error 0x{rc:X})"
        )));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err(CoreError::Cancelled);
    }
    info!("moved {} item(s) to the Recycle Bin", paths.len());
    Ok(paths.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn write(p: &Path, n: usize) {
        if let Some(d) = p.parent() {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(p, vec![b'x'; n]).unwrap();
    }

    fn scan_quiet(root: &Path) -> DiskReport {
        scan(root, &AtomicBool::new(false), &mut |_| {}).unwrap()
    }

    /// Make a junction with `mklink /J` (no admin needed). None if it cannot be made.
    fn junction(link: &Path, target: &Path) -> Option<()> {
        let st = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .ok()?;
        st.status.success().then_some(())
    }

    #[test]
    fn totals_and_tree() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        write(&r.join("a.bin"), 1000);
        write(&r.join("big").join("one.bin"), 3 * 1024 * 1024);
        write(&r.join("big").join("deep").join("two.bin"), 2 * 1024 * 1024);
        write(&r.join("small").join("s.txt"), 10);
        let rep = scan_quiet(r);
        assert!(!rep.cancelled);
        assert_eq!(
            rep.total_bytes,
            1000 + 3 * 1024 * 1024 + 2 * 1024 * 1024 + 10
        );
        assert_eq!(rep.total_files, 4);
        assert_eq!(rep.nodes[0].size, rep.total_bytes);
        let big = rep.nodes.iter().find(|n| n.name == "big").unwrap();
        assert_eq!(big.size, 5 * 1024 * 1024);
        assert_eq!(big.files, 2);
        // Largest child first.
        let first = rep.nodes[0].children[0];
        assert_eq!(rep.nodes[first].name, "big");
        // 'deep' is 2 MiB so it is kept below 'big'.
        assert!(rep.nodes.iter().any(|n| n.name == "deep"));
        assert_eq!(rep.top_files[0].size, 3 * 1024 * 1024);
        assert!(rep.top_files.windows(2).all(|w| w[0].size >= w[1].size));
    }

    #[test]
    fn small_deep_folders_are_folded_into_parent() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("a").join("b").join("c").join("f.txt"), 5);
        let rep = scan_quiet(t.path());
        // root and a (depth <= 1) are kept; a and a\c are small and deep.
        assert!(rep.nodes.iter().any(|n| n.name == "a"));
        assert!(!rep.nodes.iter().any(|n| n.name == "b" || n.name == "c"));
        assert_eq!(rep.folded_folders, 2);
        assert_eq!(rep.total_bytes, 5);
        assert_eq!(rep.total_folders, 3);
    }

    #[test]
    fn only_the_largest_100_files_are_kept() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..130usize {
            write(&t.path().join(format!("f{i:03}.bin")), 10 + i);
        }
        let rep = scan_quiet(t.path());
        assert_eq!(rep.top_files.len(), TOP_FILES);
        assert_eq!(rep.top_files[0].size, 10 + 129);
        assert_eq!(rep.top_files[99].size, 10 + 30);
        assert_eq!(rep.total_files, 130);
    }

    #[test]
    fn junctions_are_skipped_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("huge.bin"), 4096);
        write(&t.path().join("real.bin"), 100);
        if junction(&t.path().join("link"), outside.path()).is_none() {
            eprintln!("mklink /J unavailable; skipping");
            return;
        }
        let rep = scan_quiet(t.path());
        assert_eq!(rep.total_bytes, 100, "junction target must not be counted");
        assert_eq!(rep.skipped_links, 1);
        assert!(!rep.top_files.iter().any(|f| f.path.ends_with("huge.bin")));
    }

    #[test]
    fn cancel_stops_early_and_is_reported() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("a").join("x.bin"), 10);
        let cancel = AtomicBool::new(true);
        let rep = scan(t.path(), &cancel, &mut |_| {}).unwrap();
        assert!(rep.cancelled);
        assert_eq!(rep.total_files, 0);
    }

    #[test]
    fn scanning_a_file_or_relative_path_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f.txt");
        write(&f, 1);
        assert!(scan(&f, &AtomicBool::new(false), &mut |_| {}).is_err());
        assert!(scan(Path::new("relative"), &AtomicBool::new(false), &mut |_| {}).is_err());
        assert!(scan(
            &t.path().join("missing"),
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn progress_reports_final_counts() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("a").join("x.bin"), 10);
        write(&t.path().join("y.bin"), 20);
        let mut last = ScanProgress::default();
        scan(t.path(), &AtomicBool::new(false), &mut |p| last = p.clone()).unwrap();
        assert_eq!((last.files, last.bytes, last.folders), (2, 30, 1));
    }

    #[test]
    fn protected_locations_by_name() {
        let sys = Some('c');
        for bad in [
            r"C:\",
            r"C:\Windows",
            r"C:\Windows\System32\notepad.exe",
            r"c:\WINDOWS\Temp",
            r"C:\Program Files\App\a.exe",
            r"C:\Program Files (x86)\App",
            r"D:\Program Files\Thing",
            r"C:\ProgramData\Microsoft",
            r"C:\Users",
            r"C:\Users\bob",
            r"C:\Games",
            r"C:\$Recycle.Bin\S-1-5",
            r"C:\pagefile.sys",
            r"D:\pagefile.sys",
            r"\\?\C:\Windows\Fonts",
            r"C:\Users\bob\..\..\Windows",
        ] {
            assert!(
                protected_reason_for(Path::new(bad), sys).is_some(),
                "{bad} should be refused"
            );
        }
        for ok in [
            r"C:\Users\bob\Downloads\big.iso",
            r"C:\Users\bob\Videos",
            r"D:\Games",
            r"D:\Games\Big\data.pak",
            r"E:\Backups\old",
        ] {
            assert_eq!(
                protected_reason_for(Path::new(ok), sys),
                None,
                "{ok} should be allowed by name"
            );
        }
    }

    #[test]
    fn check_deletable_accepts_normal_files_and_folders() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        write(&root.join("sub").join("f.bin"), 5);
        assert!(check_deletable(&root, &root.join("sub").join("f.bin")).is_ok());
        assert!(check_deletable(&root, &root.join("sub")).is_ok());
    }

    #[test]
    fn check_deletable_refuses_outside_root_and_root_itself() {
        let t = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        write(&other.path().join("o.bin"), 5);
        write(&root.join("in.bin"), 5);
        let e = check_deletable(&root, &other.path().join("o.bin")).unwrap_err();
        assert!(matches!(e, CoreError::Blocked(_)));
        assert!(check_deletable(&root, &root).is_err());
        assert!(check_deletable(&root, Path::new("in.bin")).is_err());
        // '..' tricks do not escape the scanned folder.
        let tricky = root
            .join("..")
            .join(other.path().file_name().unwrap())
            .join("o.bin");
        assert!(check_deletable(&root, &tricky).is_err());
        // Missing file.
        assert!(check_deletable(&root, &root.join("gone.bin")).is_err());
    }

    #[test]
    fn check_deletable_refuses_windows_and_roots() {
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let win = PathBuf::from(win);
        assert!(check_deletable(Path::new(r"C:\"), &win).is_err());
        assert!(check_deletable(&win, &win.join("System32")).is_err());
        assert!(check_deletable(Path::new(r"C:\"), Path::new(r"C:\")).is_err());
        assert!(check_deletable(
            Path::new(r"C:\Program Files"),
            Path::new(r"C:\Program Files\x")
        )
        .is_err());
    }

    #[test]
    fn check_deletable_refuses_links_and_paths_through_links() {
        let t = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        write(&inner.path().join("secret.bin"), 5);
        let link = root.join("link");
        if junction(&link, inner.path()).is_none() {
            eprintln!("mklink /J unavailable; skipping");
            return;
        }
        assert!(check_deletable(&root, &link).is_err());
        assert!(check_deletable(&root, &link.join("secret.bin")).is_err());
    }

    #[test]
    fn check_deletable_refuses_system_files() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("sys.dat");
        write(&f, 5);
        let ok = Command::new("attrib").arg("+s").arg(&f).output().unwrap();
        if !ok.status.success() {
            eprintln!("attrib unavailable; skipping");
            return;
        }
        let r = check_deletable(t.path(), &f);
        let _ = Command::new("attrib").arg("-s").arg(&f).output();
        assert!(matches!(r, Err(CoreError::Blocked(_))));
    }

    #[test]
    fn recycle_validates_before_moving_anything() {
        // A refused path means an error and (importantly) nothing is moved.
        let t = tempfile::tempdir().unwrap();
        let keep = t.path().join("keep.bin");
        write(&keep, 5);
        let win = PathBuf::from(r"C:\Windows");
        let r = recycle(t.path(), &[keep.clone(), win]);
        assert!(r.is_err());
        assert!(keep.exists());
        assert_eq!(recycle(t.path(), &[]).unwrap(), 0);
    }

    #[test]
    fn scan_root_swapped_for_a_junction_is_refused() {
        let real = tempfile::tempdir().unwrap();
        write(&real.path().join("f.bin"), 5);
        let parent = tempfile::tempdir().unwrap();
        let link = parent.path().join("root");
        if junction(&link, real.path()).is_none() {
            eprintln!("mklink /J unavailable; skipping");
            return;
        }
        // Scanning a junction root is refused...
        assert!(scan(&link, &AtomicBool::new(false), &mut |_| {}).is_err());
        // ...and so is acting through one, even though the file itself is ordinary.
        let e = check_deletable(&link, &link.join("f.bin")).unwrap_err();
        assert!(e.to_string().contains("link"), "{e}");
    }

    #[test]
    fn dot_dot_and_odd_names_cannot_escape() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        write(&root.join("in.bin"), 1);
        write(&t.path().join("outside.bin"), 1);
        for p in [
            root.join("..").join("outside.bin"),
            root.join("sub").join("..").join("..").join("outside.bin"),
            PathBuf::from(format!("{}:stream", root.join("in.bin").display())),
            PathBuf::from(format!("{}.", root.join("in.bin").display())),
            PathBuf::from(format!("{} ", root.join("in.bin").display())),
        ] {
            assert!(
                check_deletable(&root, &p).is_err(),
                "{} must be refused",
                p.display()
            );
        }
        assert!(check_deletable(&root, &root.join("in.bin")).is_ok());
        // "." is normalised away by Path, so it is the same file, not an escape.
        assert!(check_deletable(&root, &root.join(".").join("in.bin")).is_ok());
    }

    #[test]
    fn short_8dot3_names_of_protected_folders_are_resolved_and_refused() {
        // PROGRA~1 is usually C:\Program Files; when 8.3 names exist the canonical check
        // catches it even though the text does not say "Program Files".
        let short = Path::new(r"C:\PROGRA~1");
        if short.exists() {
            assert!(check_deletable(Path::new(r"C:\"), short).is_err());
            assert!(check_deletable(short, &short.join("Common Files")).is_err());
        }
        assert!(check_deletable(Path::new(r"\\?\C:\"), Path::new(r"\\?\C:\Windows")).is_err());
        assert!(check_deletable(Path::new(r"\\.\C:\"), Path::new(r"\\.\C:\Windows")).is_err());
    }

    #[test]
    fn hidden_and_read_only_files_are_user_files() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("hidden.bin");
        let r = t.path().join("readonly.bin");
        write(&h, 3);
        write(&r, 4);
        let a = Command::new("attrib").arg("+h").arg(&h).output().unwrap();
        let b = Command::new("attrib").arg("+r").arg(&r).output().unwrap();
        if !(a.status.success() && b.status.success()) {
            return;
        }
        // Counted by the scan...
        let rep = scan_quiet(t.path());
        assert_eq!((rep.total_files, rep.total_bytes), (2, 7));
        // ...and allowed (to the Recycle Bin only) like any other file the user picks.
        assert!(check_deletable(t.path(), &h).is_ok());
        assert!(check_deletable(t.path(), &r).is_ok());
        let _ = Command::new("attrib").arg("-r").arg(&r).output();
    }

    #[test]
    fn plain_paths_only() {
        assert_eq!(
            plain_drive_path(Path::new(r"\\?\C:\a\b")).unwrap(),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            plain_drive_path(Path::new("D:/x/y")).unwrap(),
            PathBuf::from(r"D:\x\y")
        );
        assert!(plain_drive_path(Path::new(r"\\?\UNC\server\share\x")).is_err());
        assert!(plain_drive_path(Path::new(r"\\server\share\x")).is_err());
        assert!(plain_drive_path(Path::new(r"C:\")).is_err());
    }

    #[test]
    fn recycle_bin_settings_that_would_delete_permanently_are_refused() {
        let ok = BinPolicy {
            max_bytes: Some(1000),
            ..Default::default()
        };
        assert_eq!(bin_refusal(&ok, 10, 20), None);
        assert!(bin_refusal(&ok, 1001, 1001).is_some(), "item too big");
        assert!(bin_refusal(&ok, 600, 1200).is_some(), "batch too big");
        let nuke = BinPolicy {
            nuke_on_delete: true,
            ..Default::default()
        };
        assert!(bin_refusal(&nuke, 1, 1).unwrap().contains("turned off"));
        let pol = BinPolicy {
            no_recycle_policy: true,
            ..Default::default()
        };
        assert!(bin_refusal(&pol, 1, 1).unwrap().contains("policy"));
        // Unknown capacity: no size limit is assumed.
        assert_eq!(bin_refusal(&BinPolicy::default(), u64::MAX, u64::MAX), None);
    }

    #[test]
    fn live_bin_policy_reads_without_panicking() {
        let p = bin_policy('C');
        eprintln!("{p:?}");
    }

    #[test]
    fn folders_containing_junctions_are_not_recycled() {
        let t = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let dir = t.path().join("folder");
        write(&dir.join("a.bin"), 1);
        if junction(&dir.join("link"), other.path()).is_none() {
            return;
        }
        // check_deletable alone allows the folder; recycle refuses before moving anything.
        assert!(check_deletable(t.path(), &dir).is_ok());
        let e = recycle(t.path(), std::slice::from_ref(&dir)).unwrap_err();
        assert!(e.to_string().contains("link"), "{e}");
        assert!(dir.join("a.bin").exists());
    }
}
