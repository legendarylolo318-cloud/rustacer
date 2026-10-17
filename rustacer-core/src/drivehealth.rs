//! Drive health from WMI, read-only.
//!
//! * `root\Microsoft\Windows\Storage`: `MSFT_PhysicalDisk` (health, media and bus type) works
//!   for any user; `MSFT_StorageReliabilityCounter` (temperature, wear, power-on hours, error
//!   counts) needs administrator rights.
//! * If the Storage classes are missing or unusable, the older `Win32_DiskDrive` plus
//!   `root\wmi` `MSStorageDriver_FailurePredictStatus` (admin, SMART drives only) is used.
//!
//! Nothing here changes any setting or runs any drive command.

use crate::error::{CoreError, Result};
use crate::util::ComGuard;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{info, warn};

const STORAGE_NS: &str = "ROOT\\Microsoft\\Windows\\Storage";
const WMI_NS: &str = "ROOT\\WMI";

// ---------- model ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Warning,
    Unhealthy,
    Unknown,
}

impl Health {
    /// Shown as text next to a colour, so colour is never the only signal.
    pub fn label(self) -> &'static str {
        match self {
            Health::Healthy => "Healthy",
            Health::Warning => "Warning",
            Health::Unhealthy => "Unhealthy",
            Health::Unknown => "Unknown",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Health::Healthy => 0,
            Health::Unknown => 1,
            Health::Warning => 2,
            Health::Unhealthy => 3,
        }
    }

    /// The worse of two states (`Unknown` never hides a real problem).
    pub fn worst(self, other: Health) -> Health {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// `MSFT_PhysicalDisk.HealthStatus`.
pub fn health_from_code(code: Option<u16>) -> Health {
    match code {
        Some(0) => Health::Healthy,
        Some(1) => Health::Warning,
        Some(2) => Health::Unhealthy,
        _ => Health::Unknown,
    }
}

/// `MSFT_PhysicalDisk.MediaType`.
pub fn media_label(code: Option<u16>) -> &'static str {
    match code {
        Some(3) => "HDD",
        Some(4) => "SSD",
        Some(5) => "SCM",
        _ => "Unspecified",
    }
}

/// `MSFT_PhysicalDisk.BusType`.
pub fn bus_label(code: Option<u16>) -> &'static str {
    match code {
        Some(1) => "SCSI",
        Some(2) => "ATAPI",
        Some(3) => "ATA",
        Some(4) => "IEEE 1394",
        Some(5) => "SSA",
        Some(6) => "Fibre Channel",
        Some(7) => "USB",
        Some(8) => "RAID",
        Some(9) => "iSCSI",
        Some(10) => "SAS",
        Some(11) => "SATA",
        Some(12) => "SD",
        Some(13) => "MMC",
        Some(14) => "Virtual",
        Some(15) => "File-backed virtual",
        Some(16) => "Storage Spaces",
        Some(17) => "NVMe",
        Some(18) => "SCM",
        Some(19) => "UFS",
        _ => "Unknown",
    }
}

/// `MSFT_PhysicalDisk.OperationalStatus` codes, as text.
pub fn operational_label(code: u16) -> &'static str {
    match code {
        2 => "OK",
        3 => "Degraded",
        4 => "Stressed",
        5 => "Predictive failure",
        6 => "Error",
        8 => "Starting",
        10 => "Stopped",
        13 => "Stopping",
        0xD00D => "Lost communication",
        0xD01E => "Transient error",
        0xD01F => "In service",
        0xD020 => "Unrecognized metadata",
        _ => "Other",
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reliability {
    pub temperature_c: Option<u32>,
    pub temperature_max_c: Option<u32>,
    /// Percentage of rated life used (0 = new).
    pub wear_percent: Option<u32>,
    pub power_on_hours: Option<u64>,
    pub read_errors_total: Option<u64>,
    pub write_errors_total: Option<u64>,
    pub read_errors_uncorrected: Option<u64>,
    pub write_errors_uncorrected: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct DriveHealth {
    pub device_id: String,
    pub name: String,
    pub serial: String,
    pub firmware: String,
    pub media: &'static str,
    pub bus: &'static str,
    pub size: u64,
    /// What Windows reports in `HealthStatus` (or the failure-predict fallback).
    pub reported: Health,
    pub operational: Vec<String>,
    /// `None` when the counters could not be read (usually: not elevated).
    pub counters: Option<Reliability>,
    /// SMART failure prediction, if the drive supports it and it could be read.
    pub predict_failure: Option<bool>,
    /// Overall state after combining the report with the counters.
    pub health: Health,
    /// Plain-language reasons for anything other than Healthy.
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HealthReport {
    pub drives: Vec<DriveHealth>,
    /// Counters were readable for at least one drive.
    pub counters_available: bool,
    /// Which data source was used.
    pub source: String,
    pub notes: Vec<String>,
}

/// Combine what Windows reports with the counters. Thresholds are deliberately
/// conservative: they only ever raise a state, never hide one Windows reported.
pub fn assess(d: &mut DriveHealth) {
    let mut h = d.reported;
    let mut why: Vec<String> = Vec::new();
    match d.reported {
        Health::Warning => why.push("Windows reports this drive's health as Warning.".into()),
        Health::Unhealthy => why.push("Windows reports this drive's health as Unhealthy.".into()),
        _ => {}
    }
    for s in &d.operational {
        if s != "OK" && s != "Other" {
            why.push(format!("Operational status: {s}."));
            if s == "Predictive failure" || s == "Error" {
                h = h.worst(Health::Unhealthy);
            } else {
                h = h.worst(Health::Warning);
            }
        }
    }
    if d.predict_failure == Some(true) {
        why.push("The drive's SMART data predicts a failure.".into());
        h = h.worst(Health::Unhealthy);
    }
    if let Some(c) = &d.counters {
        let hot_limit = if d.media == "HDD" { 60 } else { 70 };
        if let Some(t) = c.temperature_c.filter(|t| *t > 0) {
            if t >= hot_limit + 10 {
                why.push(format!("Temperature is {t} \u{b0}C, which is very high."));
                h = h.worst(Health::Unhealthy);
            } else if t >= hot_limit {
                why.push(format!("Temperature is {t} \u{b0}C, which is high."));
                h = h.worst(Health::Warning);
            }
        }
        if let Some(w) = c.wear_percent {
            if w >= 100 {
                why.push("The drive has used all of its rated life.".into());
                h = h.worst(Health::Unhealthy);
            } else if w >= 90 {
                why.push(format!("{w}% of the drive's rated life is used."));
                h = h.worst(Health::Warning);
            }
        }
        let uncorrected =
            c.read_errors_uncorrected.unwrap_or(0) + c.write_errors_uncorrected.unwrap_or(0);
        if uncorrected > 0 {
            why.push(format!(
                "{uncorrected} uncorrected read/write error(s) recorded."
            ));
            h = h.worst(Health::Warning);
        }
    }
    if h == Health::Unknown {
        why.push("Windows did not report a health state for this drive.".into());
    }
    d.health = h;
    d.reasons = why;
}

// ---------- raw WMI rows ----------

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename = "MSFT_PhysicalDisk")]
#[serde(rename_all = "PascalCase")]
pub struct RawPhysicalDisk {
    pub device_id: Option<String>,
    pub friendly_name: Option<String>,
    pub serial_number: Option<String>,
    pub firmware_version: Option<String>,
    pub health_status: Option<u16>,
    pub operational_status: Option<Vec<u16>>,
    pub media_type: Option<u16>,
    pub bus_type: Option<u16>,
    pub size: Option<u64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename = "MSFT_StorageReliabilityCounter")]
#[serde(rename_all = "PascalCase")]
pub struct RawReliability {
    pub device_id: Option<String>,
    pub temperature: Option<u32>,
    pub temperature_max: Option<u32>,
    pub wear: Option<u32>,
    pub power_on_hours: Option<u64>,
    pub read_errors_total: Option<u64>,
    pub write_errors_total: Option<u64>,
    pub read_errors_uncorrected: Option<u64>,
    pub write_errors_uncorrected: Option<u64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename = "Win32_DiskDrive")]
#[serde(rename_all = "PascalCase")]
pub struct RawDiskDrive {
    pub index: Option<u32>,
    pub model: Option<String>,
    pub serial_number: Option<String>,
    pub firmware_revision: Option<String>,
    pub size: Option<u64>,
    pub interface_type: Option<String>,
    pub media_type: Option<String>,
    #[serde(rename = "PNPDeviceID")]
    pub pnp_device_id: Option<String>,
    pub status: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename = "MSStorageDriver_FailurePredictStatus")]
#[serde(rename_all = "PascalCase")]
pub struct RawPredict {
    pub instance_name: Option<String>,
    pub predict_failure: Option<bool>,
}

fn nonzero(v: Option<u32>) -> Option<u32> {
    // The counters report 0 for "not available" on many drives; a real 0 is not useful here.
    v.filter(|x| *x != 0)
}

pub fn reliability_from_raw(r: &RawReliability) -> Reliability {
    Reliability {
        temperature_c: nonzero(r.temperature),
        temperature_max_c: nonzero(r.temperature_max),
        wear_percent: r.wear.filter(|w| *w <= 100),
        power_on_hours: r.power_on_hours,
        read_errors_total: r.read_errors_total,
        write_errors_total: r.write_errors_total,
        read_errors_uncorrected: r.read_errors_uncorrected,
        write_errors_uncorrected: r.write_errors_uncorrected,
    }
}

/// Build the drive list from `MSFT_PhysicalDisk` rows, the reliability rows keyed by
/// device id (empty when unreadable), and optional failure-predict flags keyed by device id.
pub fn from_storage(
    disks: &[RawPhysicalDisk],
    counters: &HashMap<String, Reliability>,
    predict: &HashMap<String, bool>,
) -> Vec<DriveHealth> {
    let mut v: Vec<DriveHealth> = disks
        .iter()
        .map(|d| {
            let id = d.device_id.clone().unwrap_or_default();
            let mut out = DriveHealth {
                name: d
                    .friendly_name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| format!("Disk {id}")),
                serial: d
                    .serial_number
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                firmware: d
                    .firmware_version
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                media: media_label(d.media_type),
                bus: bus_label(d.bus_type),
                size: d.size.unwrap_or(0),
                reported: health_from_code(d.health_status),
                operational: d
                    .operational_status
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| operational_label(c).to_string())
                    .collect(),
                counters: counters.get(&id).cloned(),
                predict_failure: predict.get(&id).copied(),
                health: Health::Unknown,
                reasons: vec![],
                device_id: id,
            };
            assess(&mut out);
            out
        })
        .collect();
    v.sort_by(|a, b| {
        a.device_id
            .parse::<u32>()
            .unwrap_or(u32::MAX)
            .cmp(&b.device_id.parse::<u32>().unwrap_or(u32::MAX))
            .then_with(|| a.name.cmp(&b.name))
    });
    v
}

/// Fallback when the Storage classes are unavailable: `Win32_DiskDrive` rows plus
/// failure-predict rows matched by PNP device id.
pub fn from_legacy(drives: &[RawDiskDrive], predict: &[RawPredict]) -> Vec<DriveHealth> {
    let mut v: Vec<DriveHealth> = drives
        .iter()
        .map(|d| {
            let pnp = d
                .pnp_device_id
                .clone()
                .unwrap_or_default()
                .to_ascii_uppercase();
            let flag = (!pnp.is_empty())
                .then(|| {
                    predict.iter().find(|p| {
                        p.instance_name
                            .as_deref()
                            .is_some_and(|n| n.to_ascii_uppercase().starts_with(&pnp))
                    })
                })
                .flatten()
                .and_then(|p| p.predict_failure);
            let status = d.status.clone().unwrap_or_default();
            // Without SMART data fall back to the plain Win32 status string.
            let reported = match (flag, status.as_str()) {
                (Some(true), _) => Health::Unhealthy,
                (Some(false), _) => Health::Healthy,
                (None, "OK") => Health::Healthy,
                (None, "Pred Fail") => Health::Unhealthy,
                (None, "Degraded") | (None, "Error") => Health::Warning,
                _ => Health::Unknown,
            };
            let media = d
                .media_type
                .clone()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let mut out = DriveHealth {
                device_id: d.index.map(|i| i.to_string()).unwrap_or_default(),
                name: d.model.clone().unwrap_or_else(|| "Disk".into()),
                serial: d
                    .serial_number
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                firmware: d
                    .firmware_revision
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                media: if media.contains("removable") || media.contains("external") {
                    "Removable"
                } else {
                    "Unspecified"
                },
                bus: match d.interface_type.as_deref() {
                    Some("IDE") => "ATA",
                    Some("SCSI") => "SCSI",
                    Some("USB") => "USB",
                    Some("1394") => "IEEE 1394",
                    _ => "Unknown",
                },
                size: d.size.unwrap_or(0),
                reported,
                operational: vec![],
                counters: None,
                predict_failure: flag,
                health: Health::Unknown,
                reasons: vec![],
            };
            assess(&mut out);
            out
        })
        .collect();
    v.sort_by_key(|d| d.device_id.parse::<u32>().unwrap_or(u32::MAX));
    v
}

// ---------- live reading ----------

fn read_storage(con: &wmi::WMIConnection) -> Result<HealthReport> {
    let disks: Vec<RawPhysicalDisk> = con.query()?;
    if disks.is_empty() {
        return Err(CoreError::Message(
            "The Storage WMI provider returned no physical disks.".into(),
        ));
    }
    let mut notes = Vec::new();
    let mut counters: HashMap<String, Reliability> = HashMap::new();
    match con.query::<RawReliability>() {
        Ok(rows) => {
            for r in rows {
                if let Some(id) = r.device_id.clone() {
                    counters.insert(id, reliability_from_raw(&r));
                }
            }
        }
        Err(e) => {
            info!("reliability counters unavailable: {e}");
            notes.push(
                "Temperature, wear, power-on hours and error counters need administrator rights \
                 (or the drive does not provide them)."
                    .to_string(),
            );
        }
    }
    // SMART failure prediction only for drives Windows could not rate.
    let mut predict: HashMap<String, bool> = HashMap::new();
    if disks
        .iter()
        .any(|d| health_from_code(d.health_status) == Health::Unknown)
    {
        predict = legacy_predict_by_index().unwrap_or_default();
    }
    let counters_available = !counters.is_empty();
    Ok(HealthReport {
        drives: from_storage(&disks, &counters, &predict),
        counters_available,
        source: "Storage WMI (MSFT_PhysicalDisk)".into(),
        notes,
    })
}

/// failure-predict flags keyed by the physical disk number (admin; SMART drives only).
fn legacy_predict_by_index() -> Result<HashMap<String, bool>> {
    let cimv2 = wmi::WMIConnection::new()?;
    let drives: Vec<RawDiskDrive> = cimv2.query()?;
    let wmi_ns = wmi::WMIConnection::with_namespace_path(WMI_NS)?;
    let predict: Vec<RawPredict> = wmi_ns.query()?;
    let mut out = HashMap::new();
    for d in &drives {
        let (Some(i), Some(pnp)) = (d.index, d.pnp_device_id.as_deref()) else {
            continue;
        };
        let pnp = pnp.to_ascii_uppercase();
        if let Some(flag) = predict
            .iter()
            .find(|p| {
                p.instance_name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_uppercase().starts_with(&pnp))
            })
            .and_then(|p| p.predict_failure)
        {
            out.insert(i.to_string(), flag);
        }
    }
    Ok(out)
}

fn read_legacy() -> Result<HealthReport> {
    let cimv2 = wmi::WMIConnection::new()?;
    let drives: Vec<RawDiskDrive> = cimv2.query()?;
    let predict: Vec<RawPredict> = wmi::WMIConnection::with_namespace_path(WMI_NS)
        .and_then(|c| c.query())
        .unwrap_or_default();
    let mut notes = vec![
        "The Storage WMI classes are not available, so the older Win32_DiskDrive status was used."
            .to_string(),
    ];
    if predict.is_empty() {
        notes.push(
            "SMART failure prediction is unavailable (it needs administrator rights and a SATA \
             drive that reports it)."
                .into(),
        );
    }
    Ok(HealthReport {
        drives: from_legacy(&drives, &predict),
        counters_available: false,
        source: "Win32_DiskDrive + MSStorageDriver_FailurePredictStatus".into(),
        notes,
    })
}

/// Read the health of every physical drive. Call from a worker thread. Read-only.
pub fn read_health() -> Result<HealthReport> {
    let _com = ComGuard::new()?;
    let storage = wmi::WMIConnection::with_namespace_path(STORAGE_NS)
        .map_err(CoreError::from)
        .and_then(|c| read_storage(&c));
    match storage {
        Ok(r) => Ok(r),
        Err(e) => {
            warn!("storage health query failed ({e}); using the legacy classes");
            read_legacy()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(id: &str, name: &str, health: u16, media: u16, bus: u16) -> RawPhysicalDisk {
        RawPhysicalDisk {
            device_id: Some(id.into()),
            friendly_name: Some(name.into()),
            serial_number: Some("SN123 ".into()),
            firmware_version: Some("FW1".into()),
            health_status: Some(health),
            operational_status: Some(vec![2]),
            media_type: Some(media),
            bus_type: Some(bus),
            size: Some(1_000_204_886_016),
        }
    }

    fn rel(temp: u32, wear: u32, hours: u64, unc: u64) -> Reliability {
        Reliability {
            temperature_c: Some(temp),
            wear_percent: Some(wear),
            power_on_hours: Some(hours),
            read_errors_uncorrected: Some(unc),
            ..Default::default()
        }
    }

    #[test]
    fn code_labels() {
        assert_eq!(health_from_code(Some(0)), Health::Healthy);
        assert_eq!(health_from_code(Some(1)), Health::Warning);
        assert_eq!(health_from_code(Some(2)), Health::Unhealthy);
        assert_eq!(health_from_code(Some(5)), Health::Unknown);
        assert_eq!(health_from_code(None), Health::Unknown);
        assert_eq!(media_label(Some(4)), "SSD");
        assert_eq!(media_label(Some(3)), "HDD");
        assert_eq!(media_label(Some(0)), "Unspecified");
        assert_eq!(bus_label(Some(17)), "NVMe");
        assert_eq!(bus_label(Some(11)), "SATA");
        assert_eq!(bus_label(Some(7)), "USB");
        assert_eq!(bus_label(None), "Unknown");
        assert_eq!(Health::Warning.label(), "Warning");
    }

    #[test]
    fn healthy_drive_has_no_reasons() {
        let mut c = HashMap::new();
        c.insert("0".to_string(), rel(38, 3, 1200, 0));
        let v = from_storage(&[disk("0", "Fast SSD", 0, 4, 17)], &c, &HashMap::new());
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].health, Health::Healthy);
        assert!(v[0].reasons.is_empty());
        assert_eq!(v[0].serial, "SN123");
        assert_eq!(v[0].media, "SSD");
        assert_eq!(v[0].bus, "NVMe");
        assert_eq!(v[0].counters.as_ref().unwrap().power_on_hours, Some(1200));
    }

    #[test]
    fn windows_report_is_never_hidden() {
        let v = from_storage(
            &[disk("0", "A", 1, 4, 17), disk("1", "B", 2, 3, 11)],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(v[0].health, Health::Warning);
        assert_eq!(v[1].health, Health::Unhealthy);
        assert!(v[0].reasons[0].contains("Warning"));
        // Healthy counters cannot lower a reported problem.
        let mut c = HashMap::new();
        c.insert("1".to_string(), rel(30, 0, 10, 0));
        let v = from_storage(&[disk("1", "B", 2, 3, 11)], &c, &HashMap::new());
        assert_eq!(v[0].health, Health::Unhealthy);
    }

    #[test]
    fn counters_raise_the_state() {
        let mk = |temp, wear, unc, media| {
            let mut c = HashMap::new();
            c.insert("0".to_string(), rel(temp, wear, 100, unc));
            from_storage(&[disk("0", "D", 0, media, 17)], &c, &HashMap::new()).remove(0)
        };
        assert_eq!(mk(45, 10, 0, 4).health, Health::Healthy);
        assert_eq!(mk(72, 10, 0, 4).health, Health::Warning);
        assert_eq!(mk(85, 10, 0, 4).health, Health::Unhealthy);
        // HDDs run cooler: 62 is high for a hard disk but fine for an SSD.
        assert_eq!(mk(62, 10, 0, 3).health, Health::Warning);
        assert_eq!(mk(62, 10, 0, 4).health, Health::Healthy);
        assert_eq!(mk(40, 92, 0, 4).health, Health::Warning);
        assert_eq!(mk(40, 100, 0, 4).health, Health::Unhealthy);
        let e = mk(40, 10, 3, 4);
        assert_eq!(e.health, Health::Warning);
        assert!(e.reasons.iter().any(|r| r.contains("uncorrected")));
    }

    #[test]
    fn predicted_failure_and_operational_status() {
        let mut p = HashMap::new();
        p.insert("0".to_string(), true);
        let v = from_storage(&[disk("0", "D", 0, 3, 11)], &HashMap::new(), &p);
        assert_eq!(v[0].health, Health::Unhealthy);
        assert!(v[0].reasons.iter().any(|r| r.contains("SMART")));

        let mut d = disk("0", "D", 0, 4, 17);
        d.operational_status = Some(vec![3]);
        let v = from_storage(&[d], &HashMap::new(), &HashMap::new());
        assert_eq!(v[0].health, Health::Warning);
        assert!(v[0].reasons[0].contains("Degraded"));
    }

    #[test]
    fn unknown_health_is_explained() {
        let v = from_storage(
            &[disk("3", "Mystery", 5, 0, 0)],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(v[0].health, Health::Unknown);
        assert!(v[0].reasons[0].contains("did not report"));
        assert_eq!(Health::Healthy.worst(Health::Unknown), Health::Unknown);
        assert_eq!(Health::Warning.worst(Health::Unknown), Health::Warning);
    }

    #[test]
    fn zero_counters_are_treated_as_missing() {
        let raw = RawReliability {
            device_id: Some("0".into()),
            temperature: Some(0),
            wear: Some(255),
            power_on_hours: Some(0),
            ..Default::default()
        };
        let r = reliability_from_raw(&raw);
        assert_eq!(r.temperature_c, None);
        assert_eq!(r.wear_percent, None);
        assert_eq!(r.power_on_hours, Some(0));
    }

    #[test]
    fn drives_are_sorted_by_disk_number() {
        let v = from_storage(
            &[
                disk("10", "Z", 0, 4, 17),
                disk("2", "B", 0, 4, 17),
                disk("0", "A", 0, 4, 17),
            ],
            &HashMap::new(),
            &HashMap::new(),
        );
        let ids: Vec<&str> = v.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(ids, vec!["0", "2", "10"]);
    }

    #[test]
    fn legacy_fallback_uses_smart_then_status() {
        let drives = vec![
            RawDiskDrive {
                index: Some(0),
                model: Some("Old HDD".into()),
                pnp_device_id: Some(r"IDE\DISKOLD_HDD\5&1&0&0.0.0".into()),
                status: Some("OK".into()),
                interface_type: Some("IDE".into()),
                size: Some(500_107_862_016),
                ..Default::default()
            },
            RawDiskDrive {
                index: Some(1),
                model: Some("Dying HDD".into()),
                pnp_device_id: Some(r"IDE\DISKDYING\5&2&0&0.0.0".into()),
                status: Some("OK".into()),
                ..Default::default()
            },
            RawDiskDrive {
                index: Some(2),
                model: Some("USB stick".into()),
                pnp_device_id: Some(r"USBSTOR\DISK&VEN\1234".into()),
                status: Some("Pred Fail".into()),
                interface_type: Some("USB".into()),
                ..Default::default()
            },
        ];
        let predict = vec![
            RawPredict {
                instance_name: Some(r"IDE\DISKOLD_HDD\5&1&0&0.0.0_0".into()),
                predict_failure: Some(false),
            },
            RawPredict {
                instance_name: Some(r"IDE\DISKDYING\5&2&0&0.0.0_0".into()),
                predict_failure: Some(true),
            },
        ];
        let v = from_legacy(&drives, &predict);
        assert_eq!(v[0].health, Health::Healthy);
        assert_eq!(v[0].bus, "ATA");
        assert_eq!(v[0].predict_failure, Some(false));
        // SMART says failure even though Win32 status says OK.
        assert_eq!(v[1].health, Health::Unhealthy);
        // No SMART row: fall back to the status string.
        assert_eq!(v[2].health, Health::Unhealthy);
        assert_eq!(v[2].predict_failure, None);
        assert_eq!(v[2].bus, "USB");
    }
}
