use crate::error::{CoreError, Result};
use std::ffi::OsStr;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    ShellExecuteExW, ShellExecuteW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// NUL-terminated UTF-16 buffer.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Human-readable byte count in binary (IEC) units: 1 KiB = 1024 bytes.
pub fn format_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// RAII COM initialisation for a worker thread. Uninitialises on drop.
pub struct ComGuard {
    initialised: bool,
}

impl ComGuard {
    /// Multithreaded apartment: WUA, WMI and Task Scheduler on worker threads.
    pub fn new() -> Result<Self> {
        Self::init(COINIT_MULTITHREADED)
    }

    /// Single-threaded apartment, as the Shell (ShellExecute) recommends.
    pub fn sta() -> Result<Self> {
        Self::init(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
    }

    fn init(model: windows::Win32::System::Com::COINIT) -> Result<Self> {
        // SAFETY: balanced with CoUninitialize in Drop; only counted if S_OK/S_FALSE.
        let hr = unsafe { CoInitializeEx(None, model) };
        if hr.is_ok() {
            Ok(Self { initialised: true })
        } else {
            // RPC_E_CHANGED_MODE: COM already initialised with another model; usable.
            if hr.0 as u32 == 0x8001_0106 {
                Ok(Self { initialised: false })
            } else {
                Err(CoreError::Windows(format!(
                    "COM init failed (0x{:08X})",
                    hr.0 as u32
                )))
            }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialised {
            // SAFETY: matches the successful CoInitializeEx in `new`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Run a console program without flashing a window; returns (exit code, stdout+stderr).
pub fn run_hidden(program: &str, args: &[&str]) -> Result<(i32, String)> {
    let out = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text))
}

/// Like [`run_hidden`], but polls `cancel` and kills the child when it is set.
/// Returns `CoreError::Cancelled` in that case.
pub fn run_hidden_cancellable(
    program: &str,
    args: &[&str],
    cancel: &AtomicBool,
) -> Result<(i32, String)> {
    let mut child = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes on their own threads so a full pipe can never stall the child.
    fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    }
    let out_t = drain(child.stdout.take());
    let err_t = drain(child.stderr.take());
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_t.join();
            let _ = err_t.join();
            return Err(CoreError::Cancelled);
        }
        match child.try_wait()? {
            Some(st) => break st,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let mut text = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    text.push_str(&String::from_utf8_lossy(&err_t.join().unwrap_or_default()));
    Ok((status.code().unwrap_or(-1), text))
}

/// Closes a process handle on drop.
struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle came from ShellExecuteExW (SEE_MASK_NOCLOSEPROCESS) and is
            // closed exactly once, here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// ShellExecute (so UAC and the program's own window show) and wait for the launched process
/// to exit. Returns its exit code, or `None` if Windows did not hand back a process handle
/// (for example when an existing instance handled the request).
pub fn shell_execute_wait(verb: &str, file: &str, params: Option<&str>) -> Result<Option<u32>> {
    let _com = ComGuard::sta();
    let verb_w = wide(verb);
    let file_w = wide(file);
    let params_w = params.map(wide);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR::from_raw(verb_w.as_ptr()),
        lpFile: PCWSTR::from_raw(file_w.as_ptr()),
        lpParameters: params_w
            .as_ref()
            .map_or(PCWSTR::null(), |p| PCWSTR::from_raw(p.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialised and every string pointer in it refers to a live
    // NUL-terminated buffer for the duration of the call.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| CoreError::Windows(format!("Could not start '{file}': {e}")))?;
    let proc = ProcessHandle(info.hProcess);
    if proc.0.is_invalid() {
        return Ok(None);
    }
    loop {
        // SAFETY: `proc.0` is a valid process handle owned by `proc`.
        let w = unsafe { WaitForSingleObject(proc.0, 1000) };
        if w == WAIT_OBJECT_0 {
            break;
        }
        if w.0 != 0x102 {
            // Anything but WAIT_TIMEOUT is a failure of the wait itself.
            return Err(CoreError::Windows("Waiting for the program failed.".into()));
        }
    }
    let mut code = 0u32;
    // SAFETY: the process has exited and `proc.0` is still valid; `code` is a valid out pointer.
    unsafe { GetExitCodeProcess(proc.0, &mut code) }?;
    Ok(Some(code))
}

/// ShellExecute with an explicit verb ("open", "runas", ...).
pub fn shell_execute(verb: &str, file: &str, params: Option<&str>) -> Result<()> {
    // If this thread is already in the MTA, the guard is a no-op and ShellExecute still works.
    let _com = ComGuard::sta();
    let verb_w = wide(verb);
    let file_w = wide(file);
    let params_w = params.map(wide);
    let params_p = params_w
        .as_ref()
        .map_or(PCWSTR::null(), |p| PCWSTR::from_raw(p.as_ptr()));
    // SAFETY: all pointers refer to live NUL-terminated buffers for the call duration.
    let h = unsafe {
        ShellExecuteW(
            None,
            PCWSTR::from_raw(verb_w.as_ptr()),
            PCWSTR::from_raw(file_w.as_ptr()),
            params_p,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if h.0 as isize > 32 {
        Ok(())
    } else {
        Err(CoreError::Windows(format!(
            "Could not open '{file}' (ShellExecute code {})",
            h.0 as isize
        )))
    }
}

/// Open a URL or file with its default handler.
pub fn open_path(target: &str) -> Result<()> {
    shell_execute("open", target, None)
}

/// Reveal a file in Explorer.
pub fn reveal_in_explorer(path: &Path) -> Result<()> {
    let arg = format!("/select,\"{}\"", path.display());
    shell_execute("open", "explorer.exe", Some(&arg))
}

/// Split a command line into (program, args) honouring a leading quoted path.
pub fn split_command_line(cmd: &str) -> (String, String) {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (rest[..end].to_string(), rest[end + 1..].trim().to_string());
        }
    }
    // Unquoted: take up to the first ".exe" if present.
    let lower = cmd.to_ascii_lowercase();
    if let Some(i) = lower.find(".exe") {
        let end = i + 4;
        return (cmd[..end].to_string(), cmd[end..].trim().to_string());
    }
    match cmd.split_once(' ') {
        Some((a, b)) => (a.to_string(), b.trim().to_string()),
        None => (cmd.to_string(), String::new()),
    }
}

/// Expand %VAR% references using the process environment.
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_format() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn split_quoted() {
        let (p, a) = split_command_line("\"C:\\Program Files\\A\\a.exe\" --x -y");
        assert_eq!(p, "C:\\Program Files\\A\\a.exe");
        assert_eq!(a, "--x -y");
    }

    #[test]
    fn split_unquoted_exe() {
        let (p, a) = split_command_line("C:\\Tools\\b.exe /s");
        assert_eq!(p, "C:\\Tools\\b.exe");
        assert_eq!(a, "/s");
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("RUSTACER_TEST_VAR", "xyz");
        assert_eq!(expand_env("a%RUSTACER_TEST_VAR%b"), "axyzb");
        assert_eq!(expand_env("%NOPE_NOT_SET_1%"), "%NOPE_NOT_SET_1%");
        assert_eq!(expand_env("100%"), "100%");
    }
}
