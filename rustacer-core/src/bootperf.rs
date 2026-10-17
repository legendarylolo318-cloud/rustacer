//! Boot performance from the `Microsoft-Windows-Diagnostics-Performance/Operational` log.
//!
//! Event 100 records how long a boot took; event 101 names an application that slowed it.
//! Reading this log needs administrator rights. Events are read with the Windows Event Log
//! API (`EvtQuery`), falling back to `wevtutil` if that API fails for any other reason.
//! Everything here is read-only.

use crate::admin::is_elevated;
use crate::error::{CoreError, Result};
use crate::startup::StartupEntry;
use crate::util::{run_hidden, split_command_line};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::System::EventLog::{
    EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender,
    EvtRenderEventXml, EVT_HANDLE,
};

pub const CHANNEL: &str = "Microsoft-Windows-Diagnostics-Performance/Operational";
const QUERY: &str = "*[System[(EventID=100 or EventID=101)]]";
/// Newest events to read (several hundred covers a few months of daily boots).
const MAX_EVENTS: usize = 600;
const ERROR_ACCESS_DENIED: i32 = 5;

// ---------- event XML parsing (pure, unit-tested) ----------

/// One event reduced to what this module needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawEvent {
    pub id: u32,
    /// `TimeCreated` as Unix seconds (UTC); 0 if missing.
    pub time_unix: i64,
    /// `<Data Name="...">` values.
    pub data: HashMap<String, String>,
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Value of `name="..."` inside one tag's attribute text.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for q in ['"', '\''] {
        let key = format!("{name}={q}");
        if let Some(i) = tag.find(&key) {
            let rest = &tag[i + key.len()..];
            if let Some(j) = rest.find(q) {
                return Some(&rest[..j]);
            }
        }
    }
    None
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-16T08:12:45.1234567Z` -> Unix seconds.
pub fn parse_iso_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, dd) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    let time = time.trim_end_matches('Z');
    let time = time.split(['+', '-']).next()?; // ignore any offset; the log writes UTC
    let mut t = time.split(':');
    let h = t.next()?.parse::<i64>().ok()?;
    let mi = t.next()?.parse::<i64>().ok()?;
    let sec = t.next()?.split('.').next()?.parse::<i64>().ok()?;
    // A bounded year keeps the arithmetic below far from overflow on garbage input.
    if !(1601..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&dd)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&mi)
        || !(0..=60).contains(&sec)
    {
        return None;
    }
    Some(days_from_civil(y, m, dd) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Split concatenated `<Event>...</Event>` XML (as `EvtRender` and `wevtutil` give it) and
/// pull out the id, time and data fields of each event. Unparseable fragments are skipped.
pub fn parse_events(xml: &str) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Event") {
        let after = &rest[start..];
        let Some(end) = after.find("</Event>") else {
            break;
        };
        let frag = &after[..end];
        rest = &after[end + "</Event>".len()..];
        if let Some(ev) = parse_one(frag) {
            out.push(ev);
        }
    }
    out
}

fn parse_one(frag: &str) -> Option<RawEvent> {
    let id_start = frag.find("<EventID")?;
    let id_text = &frag[id_start..];
    let gt = id_text.find('>')?;
    let lt = id_text[gt..].find('<')? + gt;
    let id: u32 = id_text[gt + 1..lt].trim().parse().ok()?;

    let time_unix = frag
        .find("<TimeCreated")
        .and_then(|i| {
            let t = &frag[i..];
            let e = t.find('>')?;
            attr(&t[..e], "SystemTime")
        })
        .and_then(parse_iso_time)
        .unwrap_or(0);

    let mut data = HashMap::new();
    let mut cur = frag;
    while let Some(i) = cur.find("<Data") {
        let tag_src = &cur[i..];
        let Some(gt) = tag_src.find('>') else { break };
        let tag = &tag_src[..gt];
        let self_closing = tag.ends_with('/');
        let name = attr(tag, "Name").map(decode_entities);
        let (value, consumed) = if self_closing {
            (String::new(), gt + 1)
        } else {
            let body = &tag_src[gt + 1..];
            match body.find("</Data>") {
                Some(e) => (decode_entities(&body[..e]), gt + 1 + e + "</Data>".len()),
                None => break,
            }
        };
        if let Some(n) = name {
            data.insert(n, value.trim().to_string());
        }
        cur = &tag_src[consumed..];
    }
    Some(RawEvent {
        id,
        time_unix,
        data,
    })
}

// ---------- report ----------

/// One boot (event 100).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootRecord {
    /// When the boot finished (Unix seconds, UTC).
    pub when_unix: i64,
    /// Total boot time in milliseconds.
    pub boot_ms: u64,
    /// Time until the desktop was usable (main path), in milliseconds.
    pub main_path_ms: u64,
    /// Time spent on post-boot work, in milliseconds.
    pub post_boot_ms: u64,
    /// Windows flagged this boot as slower than usual.
    pub degraded: bool,
}

/// One "this application slowed boot" event (event 101).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowAppEvent {
    pub when_unix: i64,
    /// File name or component name as logged (for example `OneDrive.exe`).
    pub name: String,
    pub friendly_name: String,
    pub path: String,
    pub total_ms: u64,
    pub degradation_ms: u64,
}

/// A slow application summed up over all recorded boots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowApp {
    pub name: String,
    pub friendly_name: String,
    pub path: String,
    /// How many recorded boots it slowed.
    pub boots: u32,
    pub worst_degradation_ms: u64,
    pub latest_degradation_ms: u64,
    pub latest_total_ms: u64,
    pub latest_unix: i64,
}

#[derive(Debug, Clone, Default)]
pub struct BootReport {
    /// Oldest first.
    pub boots: Vec<BootRecord>,
    /// Largest latest-slowdown first.
    pub slow_apps: Vec<SlowApp>,
    /// Which reader produced the data (`EvtQuery` or `wevtutil`).
    pub source: String,
}

impl BootReport {
    /// (latest, average, best, worst) boot time in ms, or `None` with no boots.
    pub fn summary(&self) -> Option<(u64, u64, u64, u64)> {
        let last = self.boots.last()?.boot_ms;
        let all: Vec<u64> = self.boots.iter().map(|b| b.boot_ms).collect();
        let avg = all.iter().sum::<u64>() / all.len() as u64;
        Some((last, avg, *all.iter().min()?, *all.iter().max()?))
    }
}

fn num(ev: &RawEvent, key: &str) -> u64 {
    ev.data
        .get(key)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn text(ev: &RawEvent, key: &str) -> String {
    ev.data.get(key).cloned().unwrap_or_default()
}

/// The first non-empty value among `keys`.
fn first_text(ev: &RawEvent, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| text(ev, k))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Turn raw events into boots and slow-app summaries.
pub fn build_report(events: &[RawEvent], source: &str) -> BootReport {
    let mut boots = Vec::new();
    let mut slow = Vec::new();
    for ev in events {
        match ev.id {
            100 => {
                let boot_ms = num(ev, "BootTime");
                if boot_ms == 0 {
                    continue;
                }
                let when = ev
                    .data
                    .get("BootEndTime")
                    .and_then(|t| parse_iso_time(t))
                    .unwrap_or(ev.time_unix);
                boots.push(BootRecord {
                    when_unix: when,
                    boot_ms,
                    main_path_ms: num(ev, "MainPathBootTime"),
                    post_boot_ms: num(ev, "BootPostBootTime"),
                    // The manifest names it BootIsDegradation; accept the short form too.
                    degraded: first_text(ev, &["BootIsDegradation", "IsDegradation"])
                        .eq_ignore_ascii_case("true"),
                });
            }
            101 => {
                let name = text(ev, "Name");
                // Event 101 logs the executable in "Path" (older tools call it FullPath).
                let path = first_text(ev, &["Path", "FullPath"]);
                if name.is_empty() && path.is_empty() {
                    continue;
                }
                slow.push(SlowAppEvent {
                    when_unix: ev.time_unix,
                    name,
                    friendly_name: text(ev, "FriendlyName"),
                    path,
                    total_ms: num(ev, "TotalTime"),
                    degradation_ms: num(ev, "DegradationTime"),
                });
            }
            _ => {}
        }
    }
    boots.sort_by_key(|b| b.when_unix);
    BootReport {
        boots,
        slow_apps: summarise_slow_apps(&slow),
        source: source.to_string(),
    }
}

fn app_key(e: &SlowAppEvent) -> String {
    let from_path = Path::new(&e.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if from_path.is_empty() {
        e.name.to_ascii_lowercase()
    } else {
        from_path
    }
}

/// Group slow-app events by executable and sum them up.
pub fn summarise_slow_apps(events: &[SlowAppEvent]) -> Vec<SlowApp> {
    let mut map: HashMap<String, SlowApp> = HashMap::new();
    for e in events {
        let s = map.entry(app_key(e)).or_insert_with(|| SlowApp {
            name: e.name.clone(),
            friendly_name: e.friendly_name.clone(),
            path: e.path.clone(),
            boots: 0,
            worst_degradation_ms: 0,
            latest_degradation_ms: 0,
            latest_total_ms: 0,
            latest_unix: i64::MIN,
        });
        s.boots += 1;
        s.worst_degradation_ms = s.worst_degradation_ms.max(e.degradation_ms);
        if e.when_unix >= s.latest_unix {
            s.latest_unix = e.when_unix;
            s.latest_degradation_ms = e.degradation_ms;
            s.latest_total_ms = e.total_ms;
            if !e.friendly_name.is_empty() {
                s.friendly_name = e.friendly_name.clone();
            }
            if !e.path.is_empty() {
                s.path = e.path.clone();
            }
        }
    }
    let mut v: Vec<SlowApp> = map.into_values().collect();
    v.sort_by(|a, b| {
        b.latest_degradation_ms
            .cmp(&a.latest_degradation_ms)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    v
}

// ---------- linking slow apps to startup entries ----------

fn exe_stem(path_or_name: &str) -> String {
    let n = Path::new(path_or_name.trim().trim_matches('"'))
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    n.strip_suffix(".exe").map(str::to_string).unwrap_or(n)
}

/// Index of the startup entry that launches `app`, matched on the executable name (from the
/// logged path or name) against the entry's command, then on the display names.
pub fn match_startup(app: &SlowApp, entries: &[StartupEntry]) -> Option<usize> {
    let wanted: Vec<String> = [exe_stem(&app.path), exe_stem(&app.name)]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if wanted.is_empty() {
        return None;
    }
    for (i, e) in entries.iter().enumerate() {
        let (prog, _) = split_command_line(&e.command);
        let stem = exe_stem(&prog);
        if !stem.is_empty() && wanted.contains(&stem) {
            return Some(i);
        }
    }
    let names: Vec<String> = [app.friendly_name.as_str(), app.name.as_str()]
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.len() >= 3)
        .collect();
    entries.iter().position(|e| {
        let n = e.name.trim().to_ascii_lowercase();
        n.len() >= 3
            && names
                .iter()
                .any(|x| *x == n || x.strip_suffix(".exe") == Some(n.as_str()))
    })
}

// ---------- reading the log ----------

/// RAII wrapper so event-log handles are always closed.
struct EvtGuard(EVT_HANDLE);

impl Drop for EvtGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle came from EvtQuery/EvtNext and is closed exactly once.
            unsafe {
                let _ = EvtClose(self.0);
            }
        }
    }
}

fn render_xml(event: EVT_HANDLE) -> Result<String> {
    let mut used = 0u32;
    let mut props = 0u32;
    // SAFETY: a null buffer with size 0 only asks for the required size.
    let first = unsafe {
        EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            0,
            None,
            &mut used,
            &mut props,
        )
    };
    if let Err(e) = first {
        // ERROR_INSUFFICIENT_BUFFER (122) is the expected answer.
        if e.code().0 & 0xFFFF != 122 {
            return Err(CoreError::from(e));
        }
    }
    let mut buf = vec![0u16; (used as usize).div_ceil(2) + 1];
    // SAFETY: `buf` is valid for `used` bytes and outlives the call.
    unsafe {
        EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            (buf.len() * 2) as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut used,
            &mut props,
        )?;
    }
    let n = (used as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    Ok(s.trim_end_matches('\0').to_string())
}

/// Read up to `max` newest events of `channel` matching the XPath `query`, through the
/// Windows Event Log API. Read-only.
pub fn query_events(channel: &str, query: &str, max: usize) -> Result<Vec<RawEvent>> {
    let path = crate::util::wide(channel);
    let query = crate::util::wide(query);
    // SAFETY: both strings are live NUL-terminated UTF-16 buffers.
    let result = unsafe {
        EvtQuery(
            None,
            PCWSTR::from_raw(path.as_ptr()),
            PCWSTR::from_raw(query.as_ptr()),
            EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
        )
    };
    let handle = match result {
        Ok(h) => EvtGuard(h),
        Err(e) if e.code().0 & 0xFFFF == ERROR_ACCESS_DENIED => return Err(CoreError::NeedsAdmin),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    while out.len() < max {
        let mut handles = [0isize; 32];
        let mut returned = 0u32;
        // SAFETY: `handles` has room for 32 handles; `returned` receives the count.
        let r = unsafe { EvtNext(handle.0, &mut handles, 2000, 0, &mut returned) };
        let events: Vec<EvtGuard> = handles[..returned as usize]
            .iter()
            .map(|h| EvtGuard(EVT_HANDLE(*h)))
            .collect();
        for ev in &events {
            if let Ok(xml) = render_xml(ev.0) {
                out.extend(parse_events(&xml));
            }
        }
        if r.is_err() {
            // ERROR_NO_MORE_ITEMS (259) ends the result set; anything else is also an end.
            break;
        }
    }
    out.truncate(max);
    Ok(out)
}

fn read_with_wevtutil() -> Result<Vec<RawEvent>> {
    let q = format!("/q:{QUERY}");
    let c = format!("/c:{MAX_EVENTS}");
    let (code, text) = run_hidden("wevtutil", &["qe", CHANNEL, &q, &c, "/rd:true", "/f:xml"])?;
    if code != 0 {
        let lower = text.to_ascii_lowercase();
        if lower.contains("access is denied") || code == ERROR_ACCESS_DENIED {
            return Err(CoreError::NeedsAdmin);
        }
        return Err(CoreError::Message(format!(
            "wevtutil could not read the boot log: {}",
            text.trim()
        )));
    }
    Ok(parse_events(&text))
}

/// Read boot events and build the report. Needs admin; call from a worker thread.
pub fn read_boot_report() -> Result<BootReport> {
    if !is_elevated() {
        return Err(CoreError::NeedsAdmin);
    }
    let (events, source) = match query_events(CHANNEL, QUERY, MAX_EVENTS) {
        Ok(e) => (e, "Windows Event Log API"),
        Err(CoreError::NeedsAdmin) => return Err(CoreError::NeedsAdmin),
        Err(e) => {
            warn!("EvtQuery failed ({e}); falling back to wevtutil");
            (read_with_wevtutil()?, "wevtutil")
        }
    };
    let report = build_report(&events, source);
    info!(
        "boot log: {} boots, {} slow apps (via {source})",
        report.boots.len(),
        report.slow_apps.len()
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::{Hive, Source};

    const EVENT_100: &str = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System><Provider Name="Microsoft-Windows-Diagnostics-Performance" Guid="{CFC18EC0-96B1-4EBA-961B-622CAEE05B0A}"/><EventID>100</EventID><Version>2</Version><Level>4</Level><TimeCreated SystemTime="2026-10-16T08:12:45.1234567Z"/><Channel>Microsoft-Windows-Diagnostics-Performance/Operational</Channel><Computer>SECRET-PC</Computer></System><EventData><Data Name="BootTsVersion">2</Data><Data Name="BootStartTime">2026-10-16T08:10:03.100000000Z</Data><Data Name="BootEndTime">2026-10-16T08:11:45.000000000Z</Data><Data Name="SystemBootInstance">123</Data><Data Name="BootTime">42123</Data><Data Name="MainPathBootTime">30500</Data><Data Name="BootPostBootTime">11623</Data><Data Name="BootIsDegradation">true</Data></EventData></Event>"#;

    const EVENT_101: &str = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System><EventID>101</EventID><TimeCreated SystemTime="2026-10-16T08:12:46.0000000Z"/></System><EventData><Data Name="Name">OneDrive.exe</Data><Data Name="FriendlyName">Microsoft OneDrive &amp; Sync</Data><Data Name="Path">C:\Users\bob\AppData\Local\Microsoft\OneDrive\OneDrive.exe</Data><Data Name="Version">24.1</Data><Data Name="TotalTime">15200</Data><Data Name="DegradationTime">9100</Data><Data Name="Incidents">1</Data><Data Name="Empty"/></EventData></Event>"#;

    #[test]
    fn iso_time_parses() {
        assert_eq!(parse_iso_time("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso_time("1970-01-02T01:02:03Z"), Some(86_400 + 3723));
        assert_eq!(
            parse_iso_time("2026-10-16T08:12:45.1234567Z"),
            Some(1_792_138_365)
        );
        assert_eq!(parse_iso_time("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_iso_time("nonsense"), None);
        assert_eq!(parse_iso_time("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn parses_event_100() {
        let v = parse_events(EVENT_100);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 100);
        assert_eq!(v[0].time_unix, 1_792_138_365);
        assert_eq!(v[0].data["BootTime"], "42123");
        assert_eq!(v[0].data["BootIsDegradation"], "true");
    }

    #[test]
    fn parses_event_101_with_entities_and_empty_data() {
        let v = parse_events(EVENT_101);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 101);
        assert_eq!(v[0].data["FriendlyName"], "Microsoft OneDrive & Sync");
        assert_eq!(v[0].data["Empty"], "");
        assert_eq!(v[0].data["DegradationTime"], "9100");
    }

    #[test]
    fn parses_concatenated_events_and_skips_garbage() {
        let blob = format!(
            "{EVENT_100}\n<Event><System>no id</System></Event>\n{EVENT_101}\n<Event>trunc"
        );
        let v = parse_events(&blob);
        assert_eq!(v.iter().map(|e| e.id).collect::<Vec<_>>(), vec![100, 101]);
        assert!(parse_events("").is_empty());
        assert!(parse_events("not xml at all").is_empty());
    }

    #[test]
    fn report_builds_boots_and_slow_apps() {
        let e100b = EVENT_100
            .replace("42123", "30000")
            .replace("2026-10-16T08:11:45", "2026-10-10T08:11:45")
            .replace("true", "false");
        let blob = format!("{EVENT_100}{e100b}{EVENT_101}");
        let r = build_report(&parse_events(&blob), "test");
        assert_eq!(r.boots.len(), 2);
        // Oldest first.
        assert_eq!(r.boots[0].boot_ms, 30_000);
        assert!(!r.boots[0].degraded);
        assert_eq!(r.boots[1].boot_ms, 42_123);
        assert_eq!(r.boots[1].main_path_ms, 30_500);
        assert_eq!(r.boots[1].post_boot_ms, 11_623);
        assert!(r.boots[1].degraded);
        assert_eq!(r.slow_apps.len(), 1);
        assert_eq!(r.slow_apps[0].worst_degradation_ms, 9100);
        let (last, avg, best, worst) = r.summary().unwrap();
        assert_eq!((last, avg, best, worst), (42_123, 36_061, 30_000, 42_123));
    }

    #[test]
    fn zero_boot_time_events_are_ignored() {
        let bad = EVENT_100.replace("<Data Name=\"BootTime\">42123</Data>", "");
        assert!(build_report(&parse_events(&bad), "t").boots.is_empty());
        assert!(BootReport::default().summary().is_none());
    }

    fn ev(name: &str, path: &str, when: i64, deg: u64) -> SlowAppEvent {
        SlowAppEvent {
            when_unix: when,
            name: name.into(),
            friendly_name: String::new(),
            path: path.into(),
            total_ms: deg + 1000,
            degradation_ms: deg,
        }
    }

    #[test]
    fn slow_apps_are_grouped_by_executable() {
        let v = summarise_slow_apps(&[
            ev("OneDrive.exe", r"C:\x\OneDrive.exe", 100, 5000),
            ev("OneDrive.exe", r"C:\x\onedrive.exe", 200, 2000),
            ev("Teams.exe", r"C:\t\Teams.exe", 150, 3000),
        ]);
        assert_eq!(v.len(), 2);
        let od = v.iter().find(|a| a.name == "OneDrive.exe").unwrap();
        assert_eq!(od.boots, 2);
        assert_eq!(od.worst_degradation_ms, 5000);
        assert_eq!(od.latest_degradation_ms, 2000);
        // Sorted by latest slowdown: Teams (3000) before OneDrive (2000).
        assert_eq!(v[0].name, "Teams.exe");
    }

    fn entry(name: &str, command: &str) -> StartupEntry {
        StartupEntry {
            name: name.into(),
            command: command.into(),
            source: Source::Run {
                hive: Hive::User,
                wow32: false,
            },
            enabled: true,
            location: String::new(),
        }
    }

    fn app(name: &str, friendly: &str, path: &str) -> SlowApp {
        SlowApp {
            name: name.into(),
            friendly_name: friendly.into(),
            path: path.into(),
            boots: 1,
            worst_degradation_ms: 1,
            latest_degradation_ms: 1,
            latest_total_ms: 1,
            latest_unix: 0,
        }
    }

    #[test]
    fn slow_app_links_to_startup_entry() {
        let entries = vec![
            entry("Other", r#""C:\o\other.exe" --x"#),
            entry(
                "OneDrive",
                r#""C:\Users\bob\AppData\Local\Microsoft\OneDrive\OneDrive.exe" /background"#,
            ),
            entry("Chat", r"C:\Apps\chat.exe -s"),
        ];
        // By executable path.
        let a = app("OneDrive.exe", "", r"D:\elsewhere\onedrive.exe");
        assert_eq!(match_startup(&a, &entries), Some(1));
        // By name only (no path logged).
        assert_eq!(match_startup(&app("chat.exe", "", ""), &entries), Some(2));
        // By friendly name equal to the entry name.
        assert_eq!(match_startup(&app("x.exe", "Other", ""), &entries), Some(0));
        // No match.
        assert_eq!(
            match_startup(&app("zzz.exe", "Zzz", r"C:\z\zzz.exe"), &entries),
            None
        );
        assert_eq!(match_startup(&app("", "", ""), &entries), None);
    }

    #[test]
    fn real_manifest_field_names_are_read() {
        // Field names from the Microsoft-Windows-Diagnostics-Performance manifest.
        let r = build_report(&parse_events(&format!("{EVENT_100}{EVENT_101}")), "t");
        assert!(r.boots[0].degraded, "BootIsDegradation");
        assert!(r.slow_apps[0].path.ends_with("OneDrive.exe"), "Path");
        // The older spelling still works.
        let old = EVENT_101.replace("\"Path\"", "\"FullPath\"");
        let r = build_report(&parse_events(&old), "t");
        assert!(r.slow_apps[0].path.ends_with("OneDrive.exe"));
    }

    #[test]
    fn parser_never_panics_on_truncated_or_odd_xml() {
        let blob = format!("{EVENT_100}{EVENT_101}");
        // Every prefix and every suffix, cut on char boundaries.
        for (i, _) in blob.char_indices() {
            let _ = build_report(&parse_events(&blob[..i]), "t");
            let _ = build_report(&parse_events(&blob[i..]), "t");
        }
        for odd in [
            "<Event><EventID>",
            "<Event><EventID>abc</EventID></Event>",
            "<Event><EventID>100</EventID><Data Name=\"BootTime\">-5</Data></Event>",
            "<Event><EventID>100<EventID></Event>",
            "<Event><EventID>100</EventID><TimeCreated SystemTime=\"99999999999-01-01T00:00:00Z\"/></Event>",
            "<Event><EventID>101</EventID><Data Name=\"Name\"></Event>",
            "<Event><EventID>101</EventID><Data Name='Name'>x</Data></Event>",
            "<Event><EventID>4294967296</EventID></Event>",
            "</Event><Event>",
        ] {
            let _ = build_report(&parse_events(odd), "t");
        }
        assert_eq!(parse_iso_time("99999999999-01-01T00:00:00Z"), None);
        assert_eq!(parse_iso_time("2026-01-01T24:00:00Z"), None);
        assert_eq!(parse_iso_time("2026-01-01T-1:00:00Z"), None);
        assert_eq!(parse_iso_time("T"), None);
    }
}
