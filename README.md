# Rustacer

A native Windows 10/11 system optimizer, monitor and driver updater written in Rust.
It ships as a single portable `.exe` built with [egui/eframe](https://github.com/emilk/egui), so there is no WebView and no Electron.

> Screenshots: _placeholder. Add images to `docs/` and link them here._

## Inspired by Stacer

Rustacer is inspired by [Stacer](https://github.com/oguzhaninan/stacer), a Linux system optimizer written in Qt/C++.
It reimplements similar ideas for Windows from scratch. No Stacer code, icons or assets are used.
Stacer is GPL-3.0; Rustacer is MIT.

## Features

| Page | What it does |
|------|--------------|
| Dashboard | Live CPU, RAM and per-drive gauges, network up/down speed, uptime, and OS / CPU / GPU / RAM / hostname / boot-time info |
| Startup Apps | Lists Run and RunOnce entries (HKCU, HKLM and the 32-bit HKLM view), the user and all-users Startup folders, and logon tasks from Task Scheduler (outside `\Microsoft`). Run and Startup-folder entries are disabled through `StartupApproved`, the same way Task Manager does it, so you can turn them back on. Disabled RunOnce entries are parked under `HKCU\Software\Rustacer\DisabledRunOnce`. You can also add a per-user entry (program and arguments) or delete an entry |
| System Cleaner | Scans first and shows the size and file count per category. You tick categories, confirm, and it deletes. Categories: user `%TEMP%`, `C:\Windows\Temp` (admin), the Recycle Bin, the thumbnail and icon cache, Windows Error Reporting files, the Windows Update download cache (admin; `wuauserv` is stopped and restarted), and the Chrome, Edge and Firefox cache folders only. Locked files are skipped and listed |
| Driver Updater | Lists installed drivers with a search box, a toggle to hide Microsoft inbox drivers and an "old driver" flag. Also scans Windows Update for drivers, installs the ones you select after creating a restore point, backs up drivers with `pnputil`, and links to official GPU vendor pages |
| Services | Lists every Win32 service. You can start, stop or restart a service and set its start type to Automatic, Automatic (Delayed), Manual or Disabled. Critical services show a warning, and services on the protected list are blocked in the core library |
| Processes | Sortable table with search. You can end a process or a whole process tree, or open its file location. Critical system processes cannot be ended |
| Uninstaller | Apps from the HKLM, 32-bit HKLM and HKCU Uninstall keys, with duplicates removed. Runs the app's own uninstaller or its quiet uninstaller. System components and updates are hidden by default. Optionally lists winget apps and uninstalls them by winget id |
| Resources | Graphs over the last 60 s, 5 min or 30 min for per-core CPU, RAM, swap/pagefile, disk read/write and network |
| Settings | 7 themes (Dark, Light, Midnight, Nord, Dracula, Solarized Dark, Forest), refresh interval, alert thresholds for CPU, RAM and disk (shown as a Windows toast at most once every 5 minutes per alert), the "old driver" age, check for driver updates on startup, start with Windows |

Sizes are shown in binary units (KiB, MiB, GiB). The UI strings come from a string table that is ready for translation; only English exists today.

## Driver updater

- **Windows Update is the only source.** Rustacer uses the Windows Update Agent COM API with the criteria `IsInstalled=0 and IsHidden=0 and Type='Driver'`. There are no third-party driver databases, no scraping and no other download URLs anywhere in the code.
- **Nothing is installed automatically.** "Check for driver updates on startup" only scans. You tick updates and confirm, and only those updates are downloaded and installed.
- **Downgrades are flagged.** Windows Update sometimes offers a driver that is the same as or older than the one installed. Each offer gets a status (Newer, Same version, Older (downgrade) or Unknown), and only newer ones are shown by default. If you show the others and select them, the confirm dialog warns you.
- **A restore point comes first.** Before every install batch, Rustacer asks Windows to create a System Restore point. If that fails, for example because System Protection is off, you are told why and must choose to cancel or continue. Windows may limit how often restore points are created.
- **Installing needs admin.** When you aren't elevated, the install is refused up front with a "Restart as admin" hint. Downloads can be cancelled; once installation starts it runs to completion. Each update's result is shown. If a reboot is required, a **Reboot now** button appears; it asks for confirmation and then restarts after 10 s (`shutdown /a` aborts).
- **Backup** runs `pnputil /export-driver * <folder>` (admin) and shows its output and the exported package count.
- **GPU drivers.** NVIDIA, AMD and Intel GPUs are detected with their installed driver version. A button opens the vendor's official driver page (nvidia.com, amd.com or intel.com). Rustacer never downloads or installs GPU drivers itself.
- Updates are matched to installed devices by every hardware and compatible ID (`Win32_PnPEntity`), falling back to the device name when it is unambiguous.

## Safety

- **The cleaner only deletes inside whitelisted folders.**
  - Every root must be an absolute path whose last folder name is the expected one (`Temp`, `Explorer`, `WER`, `Download`, `Cache`, `cache2`, ...). A strange `%TEMP%` such as `C:\Users\you` is therefore ignored, and unset variables never become relative paths.
  - Roots that are themselves symlinks or junctions are refused, and so are drive roots and folders directly under a drive root.
  - Inside a root, symlinks and junctions are never followed or removed, and every file is canonicalized and checked again just before it is deleted.
  - In browser profiles only `Cache`, `Code Cache`, `GPUCache` (Chromium) and `cache2` (Firefox) are touched. Cookies, history, logins and profiles are never touched.
- **Protected services are enforced in the core library, not just hidden in the UI.** These cannot be stopped, restarted or disabled: RpcSs, DcomLaunch, LSM, WinDefend, EventLog, Winmgmt, PlugPlay, Power, BFE, mpssvc and others. `wuauserv` is locked while the driver updater is scanning or installing.
- **Critical processes cannot be ended:** csrss, wininit, smss, lsass, services, winlogon, System and the other kernel processes. When you end a process tree, every member is checked before anything is killed.
- Every destructive or system-changing action goes through a confirmation dialog and is logged to `%LOCALAPPDATA%\Rustacer\logs`.

## Administrator rights

The app manifest requests `asInvoker`. Rustacer runs as a normal user and detects at runtime whether it is elevated.
Admin-only actions return a clear "administrator rights are required" message, and the pages that need admin show a **Restart as admin** button that relaunches Rustacer through UAC. The admin-only actions are:
- Windows Temp and the Windows Update cache in the cleaner
- HKLM startup entries and scheduled tasks
- service control
- driver install and backup

## Build

You need Rust stable and the MSVC toolchain (`x86_64-pc-windows-msvc`).

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

The output is `target/release/rustacer.exe`.
`rustacer.exe --smoke-test` shows every page and driver tab for 2.5 s each and then exits; a panic gives a non-zero exit code.
The read-only live tests run with `cargo test -p rustacer-core --test live_readonly -- --ignored --nocapture`. They list services, drivers, apps and startup entries, scan cleaner sizes, and run a Windows Update driver search. They never change anything.

Files:
- **Config:** `%APPDATA%\Rustacer\config.toml`. An unreadable config is kept as `config.toml.bad` and replaced by the defaults.
- **Logs:** `%LOCALAPPDATA%\Rustacer\logs`.

## Project layout

- `rustacer-core` holds all the system logic and no GUI code. Modules: `cleaner`, `drivers` (`installed`, `wua`, `restore`, `backup`, `gpu`, `version`), `startup`, `services`, `processes`, `uninstaller`, `metrics`, `config`.
- `rustacer` is the egui binary.
- Every OS call runs on a worker thread that reports back over a channel, so the UI never blocks. The exceptions are the native file and folder pickers, which are modal by design.
- COM is initialized per thread through an RAII guard. The Windows Update Agent, WMI and Task Scheduler use the multithreaded apartment; ShellExecute uses a single-threaded apartment.

## Known limitations

- These paths have unit tests in temp folders but have not been run against a real system: driver installation, restore-point creation, driver backup, actual cleaning, and service or process changes.
- Disk I/O in Resources is the sum of per-process I/O, so it undercounts system-level I/O.
- The winget list is parsed by column position, so it may misparse on non-English systems.
- `egui_plot` 0.37 targets egui 0.36, so the GUI uses eframe/egui 0.36. `cargo audit` reports only an "unmaintained" notice for `ttf-parser`, which comes in through egui's font stack.

## License

MIT. See [LICENSE](LICENSE).
