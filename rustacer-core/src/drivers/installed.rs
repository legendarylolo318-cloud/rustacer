//! Installed driver inventory via WMI `Win32_PnPSignedDriver`.

use super::version::{is_old, Date};
use crate::error::Result;
use crate::util::ComGuard;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[serde(rename = "Win32_PnPSignedDriver")]
#[serde(rename_all = "PascalCase")]
struct RawDriver {
    device_name: Option<String>,
    device_class: Option<String>,
    manufacturer: Option<String>,
    driver_provider_name: Option<String>,
    driver_version: Option<String>,
    driver_date: Option<String>,
    inf_name: Option<String>,
    is_signed: Option<bool>,
    #[serde(rename = "HardWareID")]
    hardware_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InstalledDriver {
    pub device_name: String,
    pub device_class: String,
    pub provider: String,
    pub manufacturer: String,
    pub version: String,
    pub date: Option<Date>,
    pub inf_name: String,
    pub signed: Option<bool>,
    pub hardware_id: String,
}

impl InstalledDriver {
    /// Microsoft inbox drivers ship with Windows itself.
    pub fn is_inbox(&self) -> bool {
        let p = self.provider.to_ascii_lowercase();
        let m = self.manufacturer.to_ascii_lowercase();
        p.starts_with("microsoft") || (p.is_empty() && m.starts_with("microsoft"))
    }

    pub fn is_old(&self, threshold_years: u32) -> bool {
        is_old(self.date, threshold_years, Date::today())
    }
}

fn convert(r: RawDriver) -> InstalledDriver {
    InstalledDriver {
        device_name: r.device_name.unwrap_or_default(),
        device_class: r.device_class.unwrap_or_default(),
        provider: r.driver_provider_name.unwrap_or_default(),
        manufacturer: r.manufacturer.unwrap_or_default(),
        version: r.driver_version.unwrap_or_default(),
        date: r.driver_date.as_deref().and_then(Date::parse_cim),
        inf_name: r.inf_name.unwrap_or_default(),
        signed: r.is_signed,
        hardware_id: r.hardware_id.unwrap_or_default(),
    }
}

pub fn list_installed() -> Result<Vec<InstalledDriver>> {
    let _com = ComGuard::new()?;
    let con = wmi::WMIConnection::new()?;
    let raw: Vec<RawDriver> = con.query()?;
    let mut v: Vec<InstalledDriver> = raw
        .into_iter()
        .map(convert)
        .filter(|d| !d.device_name.is_empty())
        .collect();
    v.sort_by_key(|d| d.device_name.to_lowercase());
    Ok(v)
}

/// Case-insensitive filter over name, class, provider, version and INF.
pub fn matches_filter(d: &InstalledDriver, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || [&d.device_name, &d.device_class, &d.provider, &d.manufacturer, &d.version, &d.inf_name]
            .iter()
            .any(|f| f.to_lowercase().contains(&q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(provider: &str, manu: &str) -> InstalledDriver {
        InstalledDriver {
            device_name: "Dev".into(),
            provider: provider.into(),
            manufacturer: manu.into(),
            version: "1.2.3".into(),
            ..Default::default()
        }
    }

    #[test]
    fn inbox_detection() {
        assert!(d("Microsoft", "(Standard system devices)").is_inbox());
        assert!(d("", "Microsoft").is_inbox());
        assert!(!d("NVIDIA", "NVIDIA").is_inbox());
    }

    #[test]
    fn conversion_handles_nulls_and_dates() {
        let r = RawDriver {
            device_name: Some("GPU".into()),
            device_class: None,
            manufacturer: None,
            driver_provider_name: Some("NVIDIA".into()),
            driver_version: Some("31.0.15.5186".into()),
            driver_date: Some("20240102000000.000000-000".into()),
            inf_name: Some("oem5.inf".into()),
            is_signed: Some(true),
            hardware_id: None,
        };
        let c = convert(r);
        assert_eq!(c.date, Date::new(2024, 1, 2));
        assert_eq!(c.device_class, "");
        assert_eq!(c.signed, Some(true));
    }

    #[test]
    fn filter_works() {
        let x = d("Realtek", "Realtek");
        assert!(matches_filter(&x, "real"));
        assert!(matches_filter(&x, ""));
        assert!(!matches_filter(&x, "nvidia"));
    }
}
