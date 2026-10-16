# Rustacer

A native Windows system optimizer and monitor written in Rust, with a driver updater that uses **Windows Update only**.
Single portable `.exe`, built with [egui/eframe](https://github.com/emilk/egui) (no WebView, no Electron).

> Screenshots: _placeholder - add images to `docs/` and link them here._

## Inspired by Stacer

Rustacer is inspired by [Stacer](https://github.com/oguzhaninan/stacer), a Linux system optimizer written in Qt/C++.
It is a from-scratch reimplementation of similar ideas for Windows. No Stacer code, icons or assets are used
(Stacer is GPL-3.0; Rustacer is MIT).

## Features

| Page | What it does |
|------|--------------|
| Dashboard | Live CPU, RAM and per-drive gauges, network speed, uptime, OS/CPU/GPU/RAM/boot info |
| Startup Apps | Run/RunOnce keys (HKCU/HKLM, 32-bit view), user and common Startup folders, logon scheduled tasks. Disable/enable via `StartupApproved` (same as Task Manager, reversible). Add and delete entries |
| System Cleaner | Scan, review sizes, tick categories, confirm, delete. Temp, Windows Temp, Recycle Bin, thumbnail cache, WER dumps, Windows Update download cache, Chrome/Edge/Firefox **cache** directories only |
| Driver Updater | Installed drivers (WMI), Windows Update scan, install with restore point, driver backup, GPU vendor links |
| Services | List, start/stop/restart, start type incl. delayed; protected-service list |
| Processes | Sortable table, end process / tree, open file location; critical processes refused |
| Uninstaller | Uninstall-key apps (HKLM, WOW6432Node, HKCU), quiet uninstall, optional winget list |
| Resources | 60 s / 5 min / 30 min history graphs: per-core CPU, RAM, swap, disk I/O, network |
| Settings | Theme, refresh interval, alert thresholds (Windows toast), old-driver age, check drivers on startup, start with Windows |

## Build

Requires Rust stable and the MSVC toolchain (`x86_64-pc-windows-msvc`).

```
cargo build --release
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The binary is `target/release/rustacer.exe`.

Config: `%APPDATA%\Rustacer\config.toml`. Logs: `%LOCALAPPDATA%\Rustacer\logs`.

## Administrator rights

The app manifest requests `asInvoker`, so Rustacer starts as a normal user and detects elevation at runtime.
Features that need admin (Windows Temp, Windows Update cache, HKLM startup entries, scheduled tasks, service control,
driver install, driver backup) show a banner with a **Restart as admin** button (UAC prompt) instead of failing silently.

## Driver updater

- **Source:** Windows Update (Windows Update Agent COM API, criteria `IsInstalled=0 and Type='Driver'`) is the only update source.
  There are no third-party driver databases, no scraping and no unsigned drivers.
- **Nothing happens automatically.** Scans are search-only (also the optional "check on startup"). You pick updates and confirm.
- **Restore point first.** Before installing, Rustacer creates a System Restore point. If System Protection is off or the call
  fails, you are warned and choose to cancel or continue.
- **Install** downloads and installs only the selected updates, shows progress and per-update results, reports if a reboot is
  needed, and offers a confirmed **Reboot now**. Updates that require accepting a license agreement are not installed silently.
- **Backup** exports third-party drivers with `pnputil /export-driver * <folder>` (admin).
- **GPU drivers:** NVIDIA/AMD/Intel GPUs are detected and the installed version is shown, with a button that opens the vendor's
  official driver page. Rustacer never downloads or installs GPU drivers itself.
- Drivers older than N years (default 3) are flagged "old" for information only.

## Safety

- The cleaner only deletes inside whitelisted roots. Every path is canonicalized and re-checked right before deletion, and
  symlinks/junctions are never followed or removed. Cookies, history and browser profiles are never touched.
- Protected services (RpcSs, DcomLaunch, LSM, WinDefend, EventLog, Winmgmt, PlugPlay, Power, BFE, mpssvc, ...) cannot be stopped or
  disabled; `wuauserv` is locked while the driver updater works. Critical processes (csrss, wininit, smss, lsass, services, winlogon, System) cannot be ended.
- Every destructive action goes through a confirmation dialog and is logged.

## Project layout

- `rustacer-core` - all system logic, no GUI: `cleaner`, `drivers`, `startup`, `services`, `processes`, `uninstaller`, `metrics`, `config`.
- `rustacer` - the egui GUI binary. System calls run on worker threads; COM is initialized per worker thread.

## License

MIT - see [LICENSE](LICENSE).
