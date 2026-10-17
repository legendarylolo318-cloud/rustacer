//! System report export (HTML or JSON).
//!
//! The report is first built as a small value tree ([`V`]). Redaction runs over every string
//! and key in that tree, so no field can be forgotten, and then the same tree is written as
//! JSON or as one self-contained HTML page (inline CSS, no scripts, images or external
//! resources). By default the PC name and user name are hidden and `C:\Users\<name>` paths
//! are rewritten to `C:\Users\<user>`. Collecting only reads; the file is written exactly
//! where the caller says.

use crate::drivehealth::{self, HealthReport};
use crate::drivers::installed::{self, InstalledDriver};
use crate::error::{CoreError, Result};
use crate::metrics::{self, DiskUsage, SystemInfo};
use crate::startup::{self, StartupEntry};
use crate::uninstaller::{self, AppEntry};
use crate::util::{format_bytes, ComGuard};
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

// ---------- value tree ----------

#[derive(Debug, Clone, PartialEq)]
pub enum V {
    Bool(bool),
    Num(u64),
    Str(String),
    List(Vec<V>),
    /// Ordered key/value pairs.
    Map(Vec<(String, V)>),
}

fn s(x: impl Into<String>) -> V {
    V::Str(x.into())
}

fn map(pairs: Vec<(&str, V)>) -> V {
    V::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn opt_num(n: Option<impl Into<u64>>) -> V {
    n.map_or_else(|| s("n/a"), |n| V::Num(n.into()))
}

// ---------- redaction ----------

/// Replaces the PC name, the user name and `\Users\<name>` paths.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    enabled: bool,
    pc_names: Vec<String>,
    user_names: Vec<String>,
}

const KEEP_PROFILES: [&str; 4] = ["public", "default", "all users", "default user"];

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Names shorter than this are only replaced as whole words, so a user called "al" does not
/// turn every "al" inside ordinary words into a placeholder.
const MIN_SUBSTRING_LEN: usize = 4;

/// Replace case-insensitive occurrences of `needle` in `text`: anywhere for names of
/// [`MIN_SUBSTRING_LEN`] or more characters (so "AliceSvc" and "alice-sync" are caught), and
/// as whole words for shorter names.
fn replace_word(text: &str, needle: &str, with: &str) -> String {
    let needle = needle.trim();
    if needle.is_empty() {
        return text.to_string();
    }
    // Work on lowercase copies of each char so byte offsets line up with `text`.
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let n: Vec<char> = needle
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let anywhere = n.len() >= MIN_SUBSTRING_LEN;
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let hit = i + n.len() <= chars.len()
            && lower[i..i + n.len()] == n[..]
            && (anywhere
                || ((i == 0 || !is_word_char(chars[i - 1]))
                    && (i + n.len() == chars.len() || !is_word_char(chars[i + n.len()]))));
        if hit {
            out.push_str(with);
            i += n.len();
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Rewrite every `\Users\<name>` (or `/Users/<name>`) segment to `\Users\<user>`, except the
/// shared profiles (Public, Default, All Users).
fn redact_profile_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let key: Vec<char> = "users".chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let is_sep = |c: char| c == '\\' || c == '/';
        if is_sep(chars[i])
            && i + 1 + key.len() < chars.len()
            && lower[i + 1..i + 1 + key.len()] == key[..]
            && is_sep(chars[i + 1 + key.len()])
        {
            let start = i + 2 + key.len();
            let mut end = start;
            while end < chars.len() && !is_sep(chars[end]) && chars[end] != '"' {
                end += 1;
            }
            let seg: String = chars[start..end].iter().collect();
            out.extend(&chars[i..start]);
            if seg.is_empty() || KEEP_PROFILES.contains(&seg.to_lowercase().as_str()) {
                out.push_str(&seg);
            } else {
                out.push_str("<user>");
            }
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

impl Redactor {
    /// A redactor that hides the given names. Pass `enabled = false` for no redaction.
    pub fn new(enabled: bool, pc_names: Vec<String>, user_names: Vec<String>) -> Self {
        Self {
            enabled,
            pc_names,
            user_names,
        }
    }

    /// Names of this PC and its current user (host name, `COMPUTERNAME`, `USERNAME`, ...).
    pub fn for_this_pc(enabled: bool, host_name: &str) -> Self {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let mut pcs = vec![host_name.to_string(), env("COMPUTERNAME")];
        let mut users = vec![env("USERNAME")];
        // A home-domain name often equals the PC name.
        pcs.push(env("USERDOMAIN"));
        if let Some(name) = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        {
            users.push(name);
        }
        let dedupe = |v: &mut Vec<String>| {
            v.retain(|x| !x.trim().is_empty());
            v.sort_by_key(|x| std::cmp::Reverse(x.len()));
            v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        };
        dedupe(&mut pcs);
        dedupe(&mut users);
        Self::new(enabled, pcs, users)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn apply(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        // Profile paths first: they also cover accounts other than the current user.
        let mut out = redact_profile_paths(text);
        // PC names first: they often contain the user name ("ALICE-PC").
        for p in &self.pc_names {
            out = replace_word(&out, p, "<pc-name>");
        }
        for u in &self.user_names {
            out = replace_word(&out, u, "<user>");
        }
        out
    }

    /// Redact every string and every key in the tree.
    pub fn apply_tree(&self, v: &mut V) {
        match v {
            V::Str(t) => *t = self.apply(t),
            V::List(items) => items.iter_mut().for_each(|i| self.apply_tree(i)),
            V::Map(pairs) => {
                for (k, val) in pairs.iter_mut() {
                    *k = self.apply(k);
                    self.apply_tree(val);
                }
            }
            V::Bool(_) | V::Num(_) => {}
        }
    }
}

// ---------- time ----------

/// Inverse of "days since 1970-01-01" for the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-10-17 12:30:05 UTC`.
pub fn format_unix_utc(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

// ---------- collected data ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRow {
    pub name: String,
    pub display_name: String,
    pub state: String,
    pub start_mode: String,
    pub path: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename = "Win32_Service")]
#[serde(rename_all = "PascalCase")]
pub struct RawService {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub state: Option<String>,
    pub start_mode: Option<String>,
    pub path_name: Option<String>,
}

/// Services whose program is outside the Windows folder, i.e. installed by other software.
pub fn third_party_services(rows: &[RawService], windir: &str) -> Vec<ServiceRow> {
    let win = windir.trim_end_matches('\\').to_ascii_lowercase();
    let mut v: Vec<ServiceRow> = rows
        .iter()
        .filter_map(|r| {
            let path = r.path_name.clone().unwrap_or_default();
            let p = path.trim().trim_start_matches('"').to_ascii_lowercase();
            let in_windows = p.starts_with(&format!("{win}\\")) || p.starts_with("\\systemroot\\");
            (!p.is_empty() && !in_windows).then(|| ServiceRow {
                name: r.name.clone().unwrap_or_default(),
                display_name: r.display_name.clone().unwrap_or_default(),
                state: r.state.clone().unwrap_or_default(),
                start_mode: r.start_mode.clone().unwrap_or_default(),
                path,
            })
        })
        .collect();
    v.sort_by_key(|r| r.display_name.to_lowercase());
    v
}

#[derive(Debug, Clone)]
pub struct ReportData {
    pub generated_unix: i64,
    pub version: String,
    pub pc_name: String,
    pub user_name: String,
    pub info: SystemInfo,
    pub disks: Vec<DiskUsage>,
    pub health: HealthReport,
    pub drivers: Vec<InstalledDriver>,
    pub startup: Vec<StartupEntry>,
    pub apps: Vec<AppEntry>,
    /// `None` when the user did not ask for services.
    pub services: Option<Vec<ServiceRow>>,
    /// Sections that could not be read.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Html,
    Json,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Html => "html",
            Format::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub format: Format,
    /// Keep the PC name and user name (off by default: they are redacted).
    pub include_identity: bool,
    pub include_services: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: Format::Html,
            include_identity: false,
            include_services: false,
        }
    }
}

/// Build the (unredacted) value tree. Pure.
pub fn build_tree(d: &ReportData, include_services: bool) -> V {
    let mut sections: Vec<(&str, V)> = Vec::new();
    sections.push((
        "report",
        map(vec![
            ("generator", s(format!("Rustacer {}", d.version))),
            ("generated", s(format_unix_utc(d.generated_unix))),
        ]),
    ));
    sections.push((
        "system",
        map(vec![
            ("pc_name", s(&d.pc_name)),
            ("user_name", s(&d.user_name)),
            ("os", s(&d.info.os)),
            ("os_build", s(&d.info.os_build)),
            ("cpu", s(&d.info.cpu_model)),
            ("physical_cores", V::Num(d.info.physical_cores as u64)),
            ("logical_cores", V::Num(d.info.logical_cores as u64)),
            ("gpus", V::List(d.info.gpus.iter().map(s).collect())),
            ("total_ram", s(format_bytes(d.info.total_ram))),
            ("total_ram_bytes", V::Num(d.info.total_ram)),
            ("booted", s(format_unix_utc(d.info.boot_time_unix as i64))),
        ]),
    ));
    sections.push((
        "drives",
        V::List(
            d.disks
                .iter()
                .map(|x| {
                    map(vec![
                        ("mount", s(&x.mount)),
                        ("label", s(&x.name)),
                        ("size", s(format_bytes(x.total))),
                        ("free", s(format_bytes(x.available))),
                        ("used_percent", V::Num(x.used_percent().round() as u64)),
                        ("size_bytes", V::Num(x.total)),
                        ("free_bytes", V::Num(x.available)),
                    ])
                })
                .collect(),
        ),
    ));
    sections.push((
        "drive_health",
        V::List(
            d.health
                .drives
                .iter()
                .map(|x| {
                    let c = x.counters.clone().unwrap_or_default();
                    map(vec![
                        ("drive", s(&x.name)),
                        ("health", s(x.health.label())),
                        ("windows_reports", s(x.reported.label())),
                        ("type", s(format!("{} {}", x.media, x.bus))),
                        ("size", s(format_bytes(x.size))),
                        ("temperature_c", opt_num(c.temperature_c)),
                        ("wear_percent", opt_num(c.wear_percent)),
                        ("power_on_hours", opt_num(c.power_on_hours)),
                        ("read_errors", opt_num(c.read_errors_total)),
                        ("write_errors", opt_num(c.write_errors_total)),
                        ("reasons", V::List(x.reasons.iter().map(s).collect())),
                    ])
                })
                .collect(),
        ),
    ));
    sections.push((
        "installed_drivers",
        V::List(
            d.drivers
                .iter()
                .map(|x| {
                    map(vec![
                        ("device", s(&x.device_name)),
                        ("class", s(&x.device_class)),
                        (
                            "provider",
                            s(if x.provider.is_empty() {
                                &x.manufacturer
                            } else {
                                &x.provider
                            }),
                        ),
                        ("version", s(&x.version)),
                        ("date", s(x.date.map(|d| d.to_string()).unwrap_or_default())),
                        ("inf", s(&x.inf_name)),
                        ("signed", x.signed.map_or_else(|| s("unknown"), V::Bool)),
                    ])
                })
                .collect(),
        ),
    ));
    sections.push((
        "startup",
        V::List(
            d.startup
                .iter()
                .map(|x| {
                    map(vec![
                        ("name", s(&x.name)),
                        ("enabled", V::Bool(x.enabled)),
                        ("source", s(x.source.label())),
                        ("command", s(&x.command)),
                    ])
                })
                .collect(),
        ),
    ));
    sections.push((
        "installed_apps",
        V::List(
            d.apps
                .iter()
                .map(|x| {
                    map(vec![
                        ("name", s(&x.name)),
                        ("publisher", s(&x.publisher)),
                        ("version", s(&x.version)),
                        ("installed", s(&x.install_date)),
                        (
                            "size",
                            s(if x.size > 0 {
                                format_bytes(x.size)
                            } else {
                                String::new()
                            }),
                        ),
                    ])
                })
                .collect(),
        ),
    ));
    if include_services {
        sections.push((
            "third_party_services",
            V::List(
                d.services
                    .iter()
                    .flatten()
                    .map(|x| {
                        map(vec![
                            ("name", s(&x.name)),
                            ("display_name", s(&x.display_name)),
                            ("state", s(&x.state)),
                            ("start_mode", s(&x.start_mode)),
                            ("program", s(&x.path)),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    sections.push(("notes", V::List(d.notes.iter().map(s).collect())));
    map(sections)
}

/// Build the tree, hide the identity when asked, and apply the redactor to everything.
pub fn prepare(d: &ReportData, opts: &Options, red: &Redactor) -> V {
    let mut tree = build_tree(d, opts.include_services);
    if red.enabled() && !opts.include_identity {
        red.apply_tree(&mut tree);
        // The identity fields are hidden outright, not just token-replaced.
        if let V::Map(top) = &mut tree {
            for (k, v) in top.iter_mut() {
                if k == "system" {
                    if let V::Map(sys) = v {
                        for (sk, sv) in sys.iter_mut() {
                            if sk == "pc_name" || sk == "user_name" {
                                *sv = s("<redacted>");
                            }
                        }
                    }
                }
            }
        }
        if let V::Map(top) = &mut tree {
            if let Some((_, V::Map(meta))) = top.iter_mut().find(|(k, _)| k == "report") {
                meta.push(("redacted".into(), V::Bool(true)));
            }
        }
    } else if let V::Map(top) = &mut tree {
        if let Some((_, V::Map(meta))) = top.iter_mut().find(|(k, _)| k == "report") {
            meta.push(("redacted".into(), V::Bool(false)));
        }
    }
    tree
}

// ---------- JSON ----------

fn json_escape(t: &str, out: &mut String) {
    out.push('"');
    for c in t.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_value(v: &V, depth: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match v {
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Num(n) => out.push_str(&n.to_string()),
        V::Str(t) => json_escape(t, out),
        V::List(items) if items.is_empty() => out.push_str("[]"),
        V::List(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                json_value(item, depth + 1, out);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(depth));
            out.push(']');
        }
        V::Map(pairs) if pairs.is_empty() => out.push_str("{}"),
        V::Map(pairs) => {
            out.push_str("{\n");
            for (i, (k, val)) in pairs.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                json_escape(k, out);
                out.push_str(": ");
                json_value(val, depth + 1, out);
                out.push_str(if i + 1 < pairs.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(depth));
            out.push('}');
        }
    }
}

pub fn to_json(tree: &V) -> String {
    let mut out = String::new();
    json_value(tree, 0, &mut out);
    out.push('\n');
    out
}

// ---------- HTML ----------

fn html_escape(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    for c in t.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `drive_health` -> `Drive health`.
fn title_case(key: &str) -> String {
    let t = key.replace('_', " ");
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn cell_text(v: &V) -> String {
    match v {
        V::Bool(b) => if *b { "Yes" } else { "No" }.to_string(),
        V::Num(n) => n.to_string(),
        V::Str(t) if t.is_empty() => "-".to_string(),
        V::Str(t) => t.clone(),
        V::List(items) if items.is_empty() => "-".to_string(),
        V::List(items) => items.iter().map(cell_text).collect::<Vec<_>>().join(", "),
        V::Map(pairs) => pairs
            .iter()
            .map(|(k, v)| format!("{k}: {}", cell_text(v)))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

const CSS: &str = "\
:root{--bg:#ffffff;--fg:#1b1f24;--muted:#57606a;--line:#d0d7de;--stripe:#f6f8fa;--ok:#116329;--warn:#8a5a00;--bad:#b3261e}\
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9aa4af;--line:#30363d;--stripe:#161b22;--ok:#56d364;--warn:#e3b341;--bad:#ff7b72}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif}\
main{max-width:1100px;margin:0 auto;padding:24px 16px 48px}\
h1{margin:0 0 4px;font-size:1.6rem}h2{margin:32px 0 8px;font-size:1.15rem;border-bottom:1px solid var(--line);padding-bottom:4px}\
.meta{color:var(--muted);margin:0 0 8px}.badge{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:1px 10px;font-size:.85rem}\
.scroll{overflow-x:auto}table{border-collapse:collapse;width:100%;font-size:.9rem}\
th,td{text-align:left;padding:5px 10px;border-bottom:1px solid var(--line);vertical-align:top;word-break:break-word}\
th{white-space:nowrap}tbody tr:nth-child(even){background:var(--stripe)}\
.kv th{width:11rem;color:var(--muted);font-weight:600}\
.healthy{color:var(--ok);font-weight:600}.warning{color:var(--warn);font-weight:600}.unhealthy{color:var(--bad);font-weight:600}\
footer{margin-top:36px;color:var(--muted);font-size:.85rem}\
@media print{body{font-size:12px}main{max-width:none}}";

fn html_table(rows: &[V], out: &mut String) {
    // Columns come from the first row; the raw "_bytes" numbers are for the JSON file only.
    let Some(V::Map(first)) = rows.first() else {
        return;
    };
    let cols: Vec<&String> = first
        .iter()
        .map(|(k, _)| k)
        .filter(|k| !k.ends_with("_bytes"))
        .collect();
    out.push_str("<div class=\"scroll\"><table><thead><tr>");
    for c in &cols {
        out.push_str(&format!(
            "<th scope=\"col\">{}</th>",
            html_escape(&title_case(c))
        ));
    }
    out.push_str("</tr></thead><tbody>");
    for r in rows {
        let V::Map(pairs) = r else { continue };
        out.push_str("<tr>");
        for c in &cols {
            let v = pairs.iter().find(|(k, _)| k == *c).map(|(_, v)| v);
            let text = v.map(cell_text).unwrap_or_else(|| "-".into());
            let class = if c.as_str() == "health" {
                format!(" class=\"{}\"", html_escape(&text.to_ascii_lowercase()))
            } else {
                String::new()
            };
            out.push_str(&format!("<td{class}>{}</td>", html_escape(&text)));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
}

pub fn to_html(tree: &V) -> String {
    let V::Map(top) = tree else {
        return String::new();
    };
    let get = |k: &str| top.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    out.push_str("<title>Rustacer system report</title>");
    out.push_str(&format!("<style>{CSS}</style></head><body><main>"));
    out.push_str("<h1>Rustacer system report</h1>");
    if let Some(V::Map(meta)) = get("report") {
        let m = |k: &str| {
            meta.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| cell_text(v))
                .unwrap_or_default()
        };
        out.push_str(&format!(
            "<p class=\"meta\">{} &middot; {}</p>",
            html_escape(&m("generator")),
            html_escape(&m("generated"))
        ));
        let redacted = matches!(
            meta.iter().find(|(n, _)| n == "redacted"),
            Some((_, V::Bool(true)))
        );
        out.push_str(&format!(
            "<p><span class=\"badge\">{}</span></p>",
            if redacted {
                "PC name and user name are hidden"
            } else {
                "Includes the PC name and user name"
            }
        ));
    }
    for (key, val) in top {
        if key == "report" {
            continue;
        }
        match val {
            V::Map(pairs) => {
                out.push_str(&format!("<h2>{}</h2>", html_escape(&title_case(key))));
                out.push_str("<div class=\"scroll\"><table class=\"kv\"><tbody>");
                for (k, v) in pairs.iter().filter(|(k, _)| !k.ends_with("_bytes")) {
                    out.push_str(&format!(
                        "<tr><th scope=\"row\">{}</th><td>{}</td></tr>",
                        html_escape(&title_case(k)),
                        html_escape(&cell_text(v))
                    ));
                }
                out.push_str("</tbody></table></div>");
            }
            V::List(rows) if key == "notes" => {
                if !rows.is_empty() {
                    out.push_str("<h2>Notes</h2><ul>");
                    for r in rows {
                        out.push_str(&format!("<li>{}</li>", html_escape(&cell_text(r))));
                    }
                    out.push_str("</ul>");
                }
            }
            V::List(rows) => {
                out.push_str(&format!(
                    "<h2>{} <span class=\"meta\">({})</span></h2>",
                    html_escape(&title_case(key)),
                    rows.len()
                ));
                if rows.is_empty() {
                    out.push_str("<p class=\"meta\">None.</p>");
                } else {
                    html_table(rows, &mut out);
                }
            }
            _ => {}
        }
    }
    out.push_str("<footer>Generated by Rustacer. This file is self-contained: it has no scripts, images or external resources.</footer>");
    out.push_str("</main></body></html>\n");
    out
}

// ---------- collecting and writing ----------

fn read_services() -> Result<Vec<RawService>> {
    let con = wmi::WMIConnection::new()?;
    Ok(con.query()?)
}

/// Read everything the report needs. Sections that fail are left empty and noted. Call from a
/// worker thread; `progress` is told which section is being read.
pub fn collect(
    include_services: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<ReportData> {
    let _com = ComGuard::new()?;
    let check = || {
        if cancel.load(Ordering::Relaxed) {
            Err(CoreError::Cancelled)
        } else {
            Ok(())
        }
    };
    let mut notes = Vec::new();
    progress("System information");
    check()?;
    let info = metrics::system_info();
    let disks = metrics::disk_usages();

    progress("Drive health");
    check()?;
    let health = drivehealth::read_health().unwrap_or_else(|e| {
        notes.push(format!("Drive health could not be read: {e}"));
        HealthReport::default()
    });
    notes.extend(health.notes.iter().cloned());

    progress("Installed drivers");
    check()?;
    let drivers: Vec<InstalledDriver> = installed::list_installed()
        .unwrap_or_else(|e| {
            notes.push(format!("Drivers could not be read: {e}"));
            vec![]
        })
        .into_iter()
        .filter(|d| !d.is_inbox())
        .collect();

    progress("Startup entries");
    check()?;
    let startup = startup::list_entries();

    progress("Installed apps");
    check()?;
    let apps: Vec<AppEntry> = uninstaller::list_installed()
        .into_iter()
        .filter(|a| !a.is_update)
        .collect();

    let services = if include_services {
        progress("Services");
        check()?;
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        match read_services() {
            Ok(rows) => Some(third_party_services(&rows, &windir)),
            Err(e) => {
                notes.push(format!("Services could not be read: {e}"));
                Some(vec![])
            }
        }
    } else {
        None
    };
    if !crate::admin::is_elevated() {
        notes.push(
            "Rustacer was not running as administrator, so drive counters (temperature, wear, errors) may be missing."
                .into(),
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(ReportData {
        generated_unix: now,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pc_name: info.hostname.clone(),
        user_name: std::env::var("USERNAME").unwrap_or_default(),
        info,
        disks,
        health,
        drivers,
        startup,
        apps,
        services,
        notes,
    })
}

/// Render the report in the chosen format.
pub fn render(d: &ReportData, opts: &Options) -> String {
    let red = Redactor::for_this_pc(!opts.include_identity, &d.pc_name);
    let tree = prepare(d, opts, &red);
    match opts.format {
        Format::Html => to_html(&tree),
        Format::Json => to_json(&tree),
    }
}

/// Collect, render and write to `path` (which the user picked in a save dialog).
pub fn export(
    path: &Path,
    opts: &Options,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    let data = collect(opts.include_services, cancel, progress)?;
    progress("Writing the file");
    let text = render(&data, opts);
    std::fs::write(path, text.as_bytes())?;
    info!(
        "system report written to {} ({:?}, identity {})",
        path.display(),
        opts.format,
        if opts.include_identity {
            "included"
        } else {
            "redacted"
        }
    );
    if opts.include_identity {
        warn!("the report includes the PC name and user name by the user's choice");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivehealth::{DriveHealth, Health, Reliability};
    use crate::drivers::version::Date;
    use crate::startup::{Hive, Source};

    fn red() -> Redactor {
        Redactor::new(true, vec!["ALICE-PC".into()], vec!["alice".into()])
    }

    // ---- redaction ----

    #[test]
    fn redacts_names_paths_and_case_variants() {
        let r = red();
        assert_eq!(
            r.apply(r"C:\Users\Alice\AppData\Local\x.exe"),
            r"C:\Users\<user>\AppData\Local\x.exe"
        );
        assert_eq!(r.apply("alice"), "<user>");
        assert_eq!(r.apply("ALICE-PC"), "<pc-name>");
        assert_eq!(r.apply("Alice's laptop"), "<user>'s laptop");
        assert_eq!(r.apply("alice@alice-pc"), "<user>@<pc-name>");
        assert_eq!(r.apply("c:/users/alice/docs"), "c:/users/<user>/docs");
        assert_eq!(
            r.apply(r#"--profile "C:\Users\alice\p""#),
            r#"--profile "C:\Users\<user>\p""#
        );
        // Other accounts' profile folders are hidden too, shared ones are not.
        assert_eq!(r.apply(r"C:\Users\bob\x"), r"C:\Users\<user>\x");
        assert_eq!(
            r.apply(r"C:\Users\Public\Desktop"),
            r"C:\Users\Public\Desktop"
        );
        assert_eq!(r.apply(r"C:\Users\John Smith\x"), r"C:\Users\<user>\x");
        // Long names are removed even inside other words (service and folder names).
        assert_eq!(r.apply("AliceSvc alice-sync"), "<user>Svc <user>-sync");
        // Short names only as whole words, so ordinary text is not mangled.
        let short = Redactor::new(true, vec![], vec!["al".into()]);
        assert_eq!(
            short.apply("al also AL's total"),
            "<user> also <user>'s total"
        );
        // Disabled redactor does nothing.
        let off = Redactor::new(false, vec!["PC".into()], vec!["alice".into()]);
        assert_eq!(off.apply("alice on PC"), "alice on PC");
    }

    #[test]
    fn redaction_handles_non_ascii_and_empty_names() {
        let r = Redactor::new(
            true,
            vec![String::new(), "  ".into()],
            vec!["Zo\u{eb}".into(), "".into()],
        );
        assert_eq!(r.apply("hello Zo\u{eb}!"), "hello <user>!");
        assert_eq!(r.apply("plain text"), "plain text");
        assert_eq!(r.apply(""), "");
    }

    fn mock() -> ReportData {
        let mk_health = |name: &str| DriveHealth {
            device_id: "0".into(),
            name: name.into(),
            serial: "SERIAL-ALICE-1".into(),
            firmware: "1".into(),
            media: "SSD",
            bus: "NVMe",
            size: 1_000_000_000_000,
            reported: Health::Healthy,
            operational: vec![],
            counters: Some(Reliability {
                temperature_c: Some(40),
                wear_percent: Some(3),
                power_on_hours: Some(1000),
                ..Default::default()
            }),
            predict_failure: None,
            health: Health::Healthy,
            reasons: vec!["Alice's drive looks fine".into()],
        };
        ReportData {
            generated_unix: 1_792_138_365,
            version: "9.9.9".into(),
            pc_name: "ALICE-PC".into(),
            user_name: "alice".into(),
            info: SystemInfo {
                os: "Windows 11 of alice".into(),
                os_build: "26100".into(),
                hostname: "ALICE-PC".into(),
                cpu_model: "Some CPU".into(),
                physical_cores: 8,
                logical_cores: 16,
                gpus: vec!["GPU for Alice".into()],
                total_ram: 32 * 1024 * 1024 * 1024,
                boot_time_unix: 1_792_000_000,
            },
            disks: vec![DiskUsage {
                mount: "C:\\".into(),
                name: "ALICE-PC system".into(),
                total: 1_000_000_000_000,
                available: 400_000_000_000,
            }],
            health: HealthReport {
                drives: vec![mk_health("Alice SSD 1TB")],
                counters_available: true,
                source: "test".into(),
                notes: vec![],
            },
            drivers: vec![InstalledDriver {
                device_name: "Alice's AirPods".into(),
                device_class: "Bluetooth".into(),
                provider: "Apple".into(),
                version: "1.0".into(),
                date: Date::parse_cim("20240101000000.000000-000"),
                inf_name: "oem1.inf".into(),
                signed: Some(true),
                ..Default::default()
            }],
            startup: vec![StartupEntry {
                name: "Alice Sync".into(),
                command: r#""C:\Users\Alice\AppData\Local\Sync\sync.exe" --user alice"#.into(),
                source: Source::Run {
                    hive: Hive::User,
                    wow32: false,
                },
                enabled: true,
                location: String::new(),
            }],
            apps: vec![AppEntry {
                name: "Alice's Tool <b>bold</b>".into(),
                publisher: "ALICE-PC Inc".into(),
                version: "2.0".into(),
                install_date: "2024-01-01".into(),
                size: 5 * 1024 * 1024,
                uninstall_string: r"C:\Users\alice\u.exe".into(),
                ..Default::default()
            }],
            services: Some(vec![ServiceRow {
                name: "AliceSvc".into(),
                display_name: "Alice Service".into(),
                state: "Running".into(),
                start_mode: "Auto".into(),
                path: r#""C:\Users\Alice\svc.exe" -k alice"#.into(),
            }]),
            notes: vec!["Note about alice".into()],
        }
    }

    fn contains_ci(hay: &str, needle: &str) -> bool {
        hay.to_lowercase().contains(&needle.to_lowercase())
    }

    #[test]
    fn redaction_removes_the_username_from_every_field() {
        let d = mock();
        for format in [Format::Json, Format::Html] {
            let opts = Options {
                format,
                include_identity: false,
                include_services: true,
            };
            let out = render_with(&d, &opts, &red());
            assert!(
                !contains_ci(&out, "alice"),
                "{format:?} output still contains the user name:\n{out}"
            );
            assert!(out.contains("&lt;user&gt;") || out.contains("<user>"));
        }
        // The tree itself, field by field.
        let tree = prepare(
            &d,
            &Options {
                include_services: true,
                ..Options::default()
            },
            &red(),
        );
        fn walk(v: &V, hits: &mut Vec<String>) {
            match v {
                V::Str(t) if contains_ci(t, "alice") => hits.push(t.clone()),
                V::List(i) => i.iter().for_each(|x| walk(x, hits)),
                V::Map(p) => {
                    for (k, val) in p {
                        if contains_ci(k, "alice") {
                            hits.push(k.clone());
                        }
                        walk(val, hits);
                    }
                }
                _ => {}
            }
        }
        let mut hits = vec![];
        walk(&tree, &mut hits);
        assert!(hits.is_empty(), "left over: {hits:?}");
    }

    fn render_with(d: &ReportData, opts: &Options, r: &Redactor) -> String {
        let tree = prepare(d, opts, r);
        match opts.format {
            Format::Html => to_html(&tree),
            Format::Json => to_json(&tree),
        }
    }

    #[test]
    fn identity_is_kept_only_when_asked() {
        let d = mock();
        let opts = Options {
            format: Format::Json,
            include_identity: true,
            include_services: false,
        };
        let out = render_with(&d, &opts, &Redactor::new(false, vec![], vec![]));
        assert!(out.contains("ALICE-PC"));
        assert!(out.contains("\"redacted\": false"));
        // Default options hide the identity fields outright.
        let hidden = render_with(&d, &Options::default(), &red());
        assert!(hidden.contains("<redacted>") || hidden.contains("&lt;redacted&gt;"));
        assert!(!hidden.contains("ALICE-PC"));
    }

    // ---- JSON ----

    /// Minimal JSON parser, enough to prove the output is well formed.
    fn parse_json(t: &str) -> std::result::Result<V, String> {
        struct P<'a> {
            b: &'a [u8],
            i: usize,
        }
        impl P<'_> {
            fn ws(&mut self) {
                while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_whitespace() {
                    self.i += 1;
                }
            }
            fn eat(&mut self, c: u8) -> std::result::Result<(), String> {
                self.ws();
                if self.b.get(self.i) == Some(&c) {
                    self.i += 1;
                    Ok(())
                } else {
                    Err(format!("expected '{}' at {}", c as char, self.i))
                }
            }
            fn string(&mut self) -> std::result::Result<String, String> {
                self.eat(b'"')?;
                let mut out = Vec::new();
                loop {
                    let c = *self.b.get(self.i).ok_or("unterminated string")?;
                    self.i += 1;
                    match c {
                        b'"' => break,
                        b'\\' => {
                            let e = *self.b.get(self.i).ok_or("bad escape")?;
                            self.i += 1;
                            match e {
                                b'"' => out.push(b'"'),
                                b'\\' => out.push(b'\\'),
                                b'n' => out.push(b'\n'),
                                b'r' => out.push(b'\r'),
                                b't' => out.push(b'\t'),
                                b'u' => {
                                    let h = std::str::from_utf8(&self.b[self.i..self.i + 4])
                                        .map_err(|e| e.to_string())?;
                                    let cp =
                                        u32::from_str_radix(h, 16).map_err(|e| e.to_string())?;
                                    self.i += 4;
                                    let ch = char::from_u32(cp).ok_or("bad codepoint")?;
                                    out.extend(ch.to_string().as_bytes());
                                }
                                _ => return Err("unknown escape".into()),
                            }
                        }
                        c if c < 0x20 => return Err("raw control character in string".into()),
                        c => out.push(c),
                    }
                }
                String::from_utf8(out).map_err(|e| e.to_string())
            }
            fn value(&mut self) -> std::result::Result<V, String> {
                self.ws();
                match self.b.get(self.i) {
                    Some(b'{') => {
                        self.i += 1;
                        let mut pairs = vec![];
                        self.ws();
                        if self.b.get(self.i) == Some(&b'}') {
                            self.i += 1;
                            return Ok(V::Map(pairs));
                        }
                        loop {
                            let k = self.string()?;
                            self.eat(b':')?;
                            pairs.push((k, self.value()?));
                            self.ws();
                            match self.b.get(self.i) {
                                Some(b',') => self.i += 1,
                                Some(b'}') => {
                                    self.i += 1;
                                    return Ok(V::Map(pairs));
                                }
                                _ => return Err(format!("bad object at {}", self.i)),
                            }
                        }
                    }
                    Some(b'[') => {
                        self.i += 1;
                        let mut items = vec![];
                        self.ws();
                        if self.b.get(self.i) == Some(&b']') {
                            self.i += 1;
                            return Ok(V::List(items));
                        }
                        loop {
                            items.push(self.value()?);
                            self.ws();
                            match self.b.get(self.i) {
                                Some(b',') => self.i += 1,
                                Some(b']') => {
                                    self.i += 1;
                                    return Ok(V::List(items));
                                }
                                _ => return Err(format!("bad array at {}", self.i)),
                            }
                        }
                    }
                    Some(b'"') => Ok(V::Str(self.string()?)),
                    Some(b't') if self.b[self.i..].starts_with(b"true") => {
                        self.i += 4;
                        Ok(V::Bool(true))
                    }
                    Some(b'f') if self.b[self.i..].starts_with(b"false") => {
                        self.i += 5;
                        Ok(V::Bool(false))
                    }
                    Some(c) if c.is_ascii_digit() => {
                        let st = self.i;
                        while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                            self.i += 1;
                        }
                        std::str::from_utf8(&self.b[st..self.i])
                            .unwrap()
                            .parse()
                            .map(V::Num)
                            .map_err(|e| format!("{e}"))
                    }
                    _ => Err(format!("unexpected input at {}", self.i)),
                }
            }
        }
        let mut p = P {
            b: t.as_bytes(),
            i: 0,
        };
        let v = p.value()?;
        p.ws();
        if p.i == t.len() {
            Ok(v)
        } else {
            Err("trailing data".into())
        }
    }

    #[test]
    fn json_output_is_well_formed_and_round_trips() {
        let d = mock();
        let tree = prepare(
            &d,
            &Options {
                include_services: true,
                ..Options::default()
            },
            &red(),
        );
        let json = to_json(&tree);
        let parsed = parse_json(&json).expect("valid JSON");
        assert_eq!(parsed, tree, "JSON must parse back to the same tree");
        // Awkward text survives.
        let nasty = V::Map(vec![(
            "k\"ey".into(),
            V::Str("line1\nline2\t\"quoted\" back\\slash \u{1}ctl \u{e9}\u{1F980}".into()),
        )]);
        assert_eq!(parse_json(&to_json(&nasty)).unwrap(), nasty);
        assert!(json.contains("\"installed_drivers\""));
        assert!(json.contains("\"third_party_services\""));
        assert!(to_json(&V::List(vec![])).trim() == "[]");
    }

    #[test]
    fn services_section_is_optional() {
        let d = mock();
        let with = render_with(
            &d,
            &Options {
                format: Format::Json,
                include_services: true,
                ..Options::default()
            },
            &red(),
        );
        let without = render_with(
            &d,
            &Options {
                format: Format::Json,
                include_services: false,
                ..Options::default()
            },
            &red(),
        );
        assert!(with.contains("third_party_services"));
        assert!(!without.contains("third_party_services"));
    }

    // ---- HTML ----

    #[test]
    fn html_is_self_contained_and_escaped() {
        let d = mock();
        let tree = prepare(
            &d,
            &Options {
                format: Format::Html,
                include_identity: true,
                include_services: true,
            },
            &Redactor::new(false, vec![], vec![]),
        );
        let html = to_html(&tree);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<style>"));
        for banned in [
            "<script", "<link", "<img", "<iframe", "<object", "<embed", " src=", " href=",
            "@import", "url(", "http://", "https://",
        ] {
            assert!(!html.to_lowercase().contains(banned), "found {banned}");
        }
        // Data is escaped, never injected as markup.
        assert!(html.contains("Alice&#39;s Tool &lt;b&gt;bold&lt;/b&gt;"));
        assert!(!html.contains("<b>bold</b>"));
        // The health state is written as text.
        assert!(html.contains(">Healthy<"));
        assert!(html.contains("Includes the PC name and user name"));
        // The raw byte counts are JSON-only.
        assert!(!html.contains("size_bytes") && !html.contains("Size bytes"));
        assert!(html.contains("2026-10-"));
    }

    #[test]
    fn html_badge_says_when_redacted() {
        let d = mock();
        let html = render_with(&d, &Options::default(), &red());
        assert!(html.contains("PC name and user name are hidden"));
    }

    // ---- helpers ----

    #[test]
    fn unix_time_formats_as_utc() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_unix_utc(951_868_800), "2000-03-01 00:00:00 UTC");
        assert_eq!(format_unix_utc(1_792_138_365), "2026-10-16 08:12:45 UTC");
        assert_eq!(format_unix_utc(1_709_164_799), "2024-02-28 23:59:59 UTC");
        assert_eq!(format_unix_utc(1_709_164_800), "2024-02-29 00:00:00 UTC");
    }

    #[test]
    fn third_party_services_exclude_windows_programs() {
        let rows = vec![
            RawService {
                name: Some("Spooler".into()),
                display_name: Some("Print Spooler".into()),
                path_name: Some(r"C:\WINDOWS\System32\spoolsv.exe".into()),
                ..Default::default()
            },
            RawService {
                name: Some("wuauserv".into()),
                path_name: Some(r"C:\WINDOWS\system32\svchost.exe -k netsvcs -p".into()),
                ..Default::default()
            },
            RawService {
                name: Some("Quoted".into()),
                path_name: Some(r#""C:\Windows\System32\x.exe" -arg"#.into()),
                ..Default::default()
            },
            RawService {
                name: Some("Drv".into()),
                path_name: Some(r"\SystemRoot\System32\drivers\x.sys".into()),
                ..Default::default()
            },
            RawService {
                name: Some("Vendor".into()),
                display_name: Some("Vendor Helper".into()),
                state: Some("Running".into()),
                start_mode: Some("Auto".into()),
                path_name: Some(r#""C:\Program Files\Vendor\helper.exe" /svc"#.into()),
            },
            RawService {
                name: Some("NoPath".into()),
                path_name: None,
                ..Default::default()
            },
        ];
        let v = third_party_services(&rows, r"C:\WINDOWS");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Vendor");
        assert_eq!(v[0].state, "Running");
    }

    #[test]
    fn export_collects_and_cancels() {
        // Cancelled before anything is read: no file is written and no section is queried.
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("r.html");
        let cancel = AtomicBool::new(true);
        let r = export(&p, &Options::default(), &cancel, &mut |_| {});
        assert!(matches!(r, Err(CoreError::Cancelled)));
        assert!(!p.exists());
    }
}
