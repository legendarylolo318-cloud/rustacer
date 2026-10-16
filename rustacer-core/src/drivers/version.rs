//! Driver version / date parsing and comparison (pure, unit-tested).

use std::cmp::Ordering;

/// A dotted numeric version such as `31.0.101.5186`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverVersion(pub Vec<u32>);

impl DriverVersion {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let parts: Option<Vec<u32>> = s.split('.').map(|p| p.trim().parse::<u32>().ok()).collect();
        parts.filter(|p| !p.is_empty()).map(DriverVersion)
    }

    /// Find the last version-looking token (>= 2 dotted numeric parts) in free text,
    /// e.g. a Windows Update title like "Intel - Net - 22.220.0.4".
    pub fn find_in_text(text: &str) -> Option<Self> {
        text.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
            .rev()
            .filter(|t| t.contains('.'))
            .find_map(|t| DriverVersion::parse(t.trim_matches(|c: char| !c.is_ascii_digit())))
    }
}

impl Ord for DriverVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        for i in 0..n {
            let a = self.0.get(i).copied().unwrap_or(0);
            let b = other.0.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                o => return o,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for DriverVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s: Vec<String> = self.0.iter().map(u32::to_string).collect();
        write!(f, "{}", s.join("."))
    }
}

/// Compare two version strings; None when either is unparseable.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(DriverVersion::parse(a)?.cmp(&DriverVersion::parse(b)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Date {
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some(Date { year, month, day })
    }

    /// WMI CIM datetime: `20230615000000.000000-000`.
    pub fn parse_cim(s: &str) -> Option<Self> {
        let d = s.get(0..8)?;
        if !d.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Date::new(d[0..4].parse().ok()?, d[4..6].parse().ok()?, d[6..8].parse().ok()?)
    }

    /// OLE automation date (days since 1899-12-30), as returned by WUA.
    pub fn from_ole(v: f64) -> Option<Self> {
        if !v.is_finite() || v < 1.0 {
            return None;
        }
        // Days since 1970-01-01 (OLE epoch is 25569 days earlier).
        let days = v.floor() as i64 - 25_569;
        Some(civil_from_days(days))
    }

    pub fn today() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        civil_from_days((secs / 86_400) as i64)
    }

    /// Whole years elapsed from `self` to `now` (0 if `self` is in the future).
    pub fn years_until(&self, now: Date) -> u32 {
        let mut y = now.year - self.year;
        if (now.month, now.day) < (self.month, self.day) {
            y -= 1;
        }
        y.max(0) as u32
    }
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> Date {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    Date {
        year: (y + i64::from(m <= 2)) as i32,
        month: m,
        day: d,
    }
}

/// Informational "old driver" check.
pub fn is_old(date: Option<Date>, threshold_years: u32, today: Date) -> bool {
    date.is_some_and(|d| d.years_until(today) >= threshold_years)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(DriverVersion::parse("31.0.101.5186").unwrap().0, vec![31, 0, 101, 5186]);
        assert!(DriverVersion::parse("").is_none());
        assert!(DriverVersion::parse("1.x.3").is_none());
    }

    #[test]
    fn compares_numerically_not_lexically() {
        use Ordering::*;
        assert_eq!(compare_versions("10.0.9", "10.0.10"), Some(Less));
        assert_eq!(compare_versions("2.0", "2.0.0.0"), Some(Equal));
        assert_eq!(compare_versions("31.0.101.5186", "31.0.101.4502"), Some(Greater));
        assert_eq!(compare_versions("abc", "1.0"), None);
    }

    #[test]
    fn finds_version_in_title() {
        let v = DriverVersion::find_in_text("Intel - Net - 22.220.0.4").unwrap();
        assert_eq!(v.to_string(), "22.220.0.4");
        assert!(DriverVersion::find_in_text("Realtek Audio").is_none());
        let v = DriverVersion::find_in_text("NVIDIA - Display - 31.0.15.5186 (WHQL)").unwrap();
        assert_eq!(v.to_string(), "31.0.15.5186");
    }

    #[test]
    fn parses_cim_dates() {
        assert_eq!(Date::parse_cim("20230615000000.000000-000"), Date::new(2023, 6, 15));
        assert!(Date::parse_cim("garbage").is_none());
        assert!(Date::parse_cim("20231315000000.000000-000").is_none());
    }

    #[test]
    fn ole_dates() {
        // 45000 = 2023-03-15
        assert_eq!(Date::from_ole(45_000.0), Date::new(2023, 3, 15));
        assert_eq!(Date::from_ole(25_569.0), Date::new(1970, 1, 1));
        assert!(Date::from_ole(0.0).is_none());
    }

    #[test]
    fn age_and_old_flag() {
        let d = Date::new(2020, 6, 15).unwrap();
        let today = Date::new(2024, 6, 14).unwrap();
        assert_eq!(d.years_until(today), 3);
        assert_eq!(d.years_until(Date::new(2024, 6, 15).unwrap()), 4);
        assert!(is_old(Some(d), 4, Date::new(2024, 6, 15).unwrap()));
        assert!(!is_old(Some(d), 4, today));
        assert!(!is_old(None, 1, today));
        assert_eq!(Date::new(2030, 1, 1).unwrap().years_until(today), 0);
    }
}
