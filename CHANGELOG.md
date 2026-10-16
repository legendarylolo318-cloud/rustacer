# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Restore points tab in the Driver Updater: lists System Restore points (admin), with a confirmed delete for each.
- `--screenshots <dir>` mode and README screenshots.

### Changed
- GPU (display) drivers are never offered or installed through Windows Update; the GPU tab links to vendor pages instead.
- CI uses Node 24 versions of the GitHub actions, and the release job runs on `ubuntu-24.04`.

### Fixed
- The cleaner could stop `wuauserv` and clear the Windows Update cache while a driver install was running. Both the cleaner and service control now refuse while a search or install is in progress.
- Drivers with placeholder dates (Intel's 1968-07-18) were flagged as old.
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
