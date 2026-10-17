# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [0.3.0] - 2026-10-17

### Added
- **Disk Analyzer** page: cancellable read-only scan of a drive or folder, sortable folder tree with size bars, the 100 largest files, Open in Explorer and Move to Recycle Bin (confirmed, never permanent; links, system files and Windows/Program Files/ProgramData locations are refused and each path is re-checked inside the scanned folder before acting).
- **Boot performance** tab in Startup Apps (admin): boot-time history from Diagnostics-Performance events 100 and 101, and the apps that slowed boot, linked to their startup entries.
- **Drive Health** page: read-only drive health from Windows storage WMI with Healthy / Warning / Unhealthy in text, plus temperature, wear, power-on hours and error counts (admin), with a SMART fallback.
- **App updates** tab in the Uninstaller: lists `winget upgrade` results and upgrades the packages you tick, one at a time through winget's own window. GPU driver packages are left out.
- **System tray mode** (off by default): Close to tray and Start minimized to tray (needs Start with Windows), a CPU/RAM tooltip, Open and Quit menu. Alerts keep working while hidden; quitting stops the background threads and removes the icon. `--tray-test` self-test.
- **Export system report** (Dashboard and Settings): HTML (self-contained) or JSON, with the PC name, user name and `C:\Users\<name>` paths redacted by default.
- **Network connections** tab in Processes: TCP and UDP endpoints for IPv4 and IPv6 with owning process, refreshed every 2 s while visible, with filter and a guarded End process.
- Ctrl+PageDown / Ctrl+PageUp cycle through all pages (Ctrl+1 to Ctrl+9 still jump to the first nine).
- Read-only live tests for drive health, the boot log, connections, the winget upgrade list, the tray and report redaction.

### Changed
- `startup::set_self_registered` takes a second argument for the tray launch flag.
- Restart as admin passes `--restarted` to the new copy.

### Fixed (pre-release audit)
- Boot performance read the wrong event fields (`IsDegradation`, `FullPath`); the manifest uses `BootIsDegradation` and `Path`, so degraded boots and path matching never worked. The parser is now fuzzed on truncated XML and rejects out-of-range timestamps.
- Move to Recycle Bin: refuses when the scanned folder was swapped for a junction after the scan, acts on the canonical path it checked, refuses alternate data streams, trailing-dot names, folders containing links, and drives where Windows would delete permanently (Recycle Bin off, policy, or item too big).
- With Close to tray on, Restart as admin left the old copy running in the tray (two processes, two icons). A second launch now shows the running copy instead of adding an icon, and test runs ignore the tray settings so they always exit.
- winget ids cut short with an ellipsis are refused.
- Link-local IPv6 addresses show their scope id.
- The HTML report escapes the health class attribute.
- `--soak-test` mode and high-contrast and 150% captures of every new page in `--resize-test`.

## [0.2.1] - 2026-10-16

### Fixed
- Tables kept the column widths from their first layout, so when maximized or fullscreen they used only part of the window, and at small sizes columns were squeezed or text wrapped. Columns now re-fit on every resize and shorten text with "…".
- Resources graphs had a fixed height; they now share the window height.
- The sidebar and the admin banner were cut off in small windows or at large text sizes; the sidebar now scrolls and the banner stacks.
- The window's size, position and maximized state were not remembered between launches.

### Added
- F11 toggles fullscreen; Esc leaves it.
- Accessibility:
  - Text and control size setting (75% to 200%; Ctrl+= / Ctrl+- / Ctrl+0 are remembered).
  - High contrast theme and a "Reduce motion" option.
  - Ctrl+1 to Ctrl+9 page shortcuts, a clearer keyboard focus outline, and a shortcuts list in Settings.
  - Screen-reader labels for the dashboard gauges.
- `--resize-test <dir>` capture mode for layout checks.

## [0.2.0] - 2026-10-16

### Added
- Restore points tab in the Driver Updater: lists System Restore points (admin), with a confirmed delete for each.
- `--screenshots <dir>` mode and README screenshots.

### Changed
- GPU (display) drivers are never offered or installed through Windows Update; the GPU tab links to vendor pages instead.
- CI uses Node 24 versions of the GitHub actions, and the release job runs on `ubuntu-24.04`.

### Fixed
- The cleaner could stop `wuauserv` and clear the Windows Update cache while a driver install was running. Both the cleaner and service control now refuse while a search or install is in progress.
- Drivers with placeholder dates (Intel's 1968-07-18) were flagged as old.
- System Restore functions were linked statically, so `rustacer.exe` (and the test binary) failed to start on Windows editions without `srclient.dll`, such as Windows Server.
- The CPU-per-core legend overflowed the Resources page on many-core CPUs.

## [0.1.0] - 2026-10-16

### Added
- **Dashboard:** CPU, RAM and per-drive gauges, network speed, uptime and system info.
- **Startup Apps:**
  - Lists Run and RunOnce entries (HKCU, HKLM and the 32-bit HKLM view), the Startup folders and logon scheduled tasks.
  - Reversible disable through `StartupApproved`; add and delete entries.
- **System Cleaner:**
  - Scan, then choose categories, then confirm. Covers temp folders, the Recycle Bin, the thumbnail cache, WER files, the Windows Update download cache and browser cache folders.
  - Only whitelisted folders are cleaned, and links are never followed.
- **Driver Updater:**
  - Lists installed drivers with a filter and an "old driver" flag.
  - Searches Windows Update with cancel support.
  - Flags downgrades and installs only the updates you select, after a restore point.
  - Backs up drivers with `pnputil` and links to official GPU vendor pages.
- **Services:** start, stop, restart and start-type changes, with a protected list enforced in the core library.
- **Processes:** sortable table, end process or tree, and open file location. Critical processes are refused.
- **Uninstaller:** apps from the registry Uninstall keys with duplicates removed, quiet uninstall, and an optional winget list.
- **Resources:** 60 s / 5 min / 30 min graphs for per-core CPU, RAM, swap, disk I/O and network.
- **Settings:**
  - 7 themes and the refresh interval.
  - CPU, RAM and disk alert thresholds shown as Windows toasts.
  - Old-driver age, check drivers on startup (scan only) and start with Windows.
- **Testing and release:**
  - `--smoke-test` flag that renders every page and exits.
  - Read-only live integration tests.
  - GitHub Actions CI and a tag-triggered release.

### Fixed (pre-release audit)
- Cleaner:
  - Refuses relative roots, link or junction roots, and roots whose folder name is not the expected one.
  - Prevents a misdirected `%TEMP%` or an unset `%LOCALAPPDATA%` from widening what gets deleted.
- Windows Update search failed with `E_POINTER` because WUA rejects null completion and progress callbacks.
- Driver updates were not matched to installed devices; they are now matched by all hardware and compatible IDs.
- Windows Update offers that are the same version or a downgrade were listed as updates. They are now hidden by default and flagged in the confirm dialog.
- Driver install no longer starts the restore-point flow when Rustacer isn't elevated.
- `pnputil` backup could hang on non-UTF-8 output or a full stderr pipe.
- Alert cooldown was keyed by message text, so the same alert repeated as the percentage changed.
- The uninstall list showed duplicates across registry hives.
- `winget` detection and ShellExecute calls ran on the UI thread.
- ShellExecute now runs in a single-threaded COM apartment.
- Sizes are labelled in IEC units.
- A corrupt config file is kept as `config.toml.bad` instead of being discarded.
- Replaced `winrt-notification`, which pulled in the old, unsound `windows` 0.24, with `tauri-winrt-notification`, and removed unused dependencies.
