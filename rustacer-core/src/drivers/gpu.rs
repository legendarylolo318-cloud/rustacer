//! GPU detection and links to official vendor driver pages.
//! Rustacer never downloads, scrapes or installs GPU drivers itself.

use super::installed::InstalledDriver;
use crate::error::{CoreError, Result};
use crate::util::open_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

impl GpuVendor {
    pub fn label(self) -> &'static str {
        match self {
            GpuVendor::Nvidia => "NVIDIA",
            GpuVendor::Amd => "AMD",
            GpuVendor::Intel => "Intel",
        }
    }

    /// Official vendor driver landing page.
    pub fn official_url(self) -> &'static str {
        match self {
            GpuVendor::Nvidia => "https://www.nvidia.com/Download/index.aspx",
            GpuVendor::Amd => "https://www.amd.com/en/support/download/drivers.html",
            GpuVendor::Intel => "https://www.intel.com/content/www/us/en/download-center/home.html",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GpuDriver {
    pub vendor: GpuVendor,
    pub name: String,
    pub version: String,
    pub date: String,
}

pub fn detect_vendor(text: &str) -> Option<GpuVendor> {
    let t = text.to_ascii_lowercase();
    if t.contains("nvidia") || t.contains("geforce") {
        Some(GpuVendor::Nvidia)
    } else if t.contains("advanced micro devices") || t.contains("amd") || t.contains("radeon") {
        Some(GpuVendor::Amd)
    } else if t.contains("intel") {
        Some(GpuVendor::Intel)
    } else {
        None
    }
}

/// Pick display adapters from the installed-driver list.
pub fn detect_gpus(drivers: &[InstalledDriver]) -> Vec<GpuDriver> {
    drivers
        .iter()
        .filter(|d| d.device_class.eq_ignore_ascii_case("DISPLAY"))
        .filter_map(|d| {
            let vendor = detect_vendor(&format!("{} {} {}", d.device_name, d.provider, d.manufacturer))?;
            Some(GpuDriver {
                vendor,
                name: d.device_name.clone(),
                version: d.version.clone(),
                date: d.date.map(|x| x.to_string()).unwrap_or_default(),
            })
        })
        .collect()
}

pub fn open_official_page(vendor: GpuVendor) -> Result<()> {
    let url = vendor.official_url();
    if !url.starts_with("https://") {
        return Err(CoreError::Blocked("Only https vendor pages may be opened.".into()));
    }
    open_path(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_detection() {
        assert_eq!(detect_vendor("NVIDIA GeForce RTX 4070"), Some(GpuVendor::Nvidia));
        assert_eq!(detect_vendor("AMD Radeon RX 7800"), Some(GpuVendor::Amd));
        assert_eq!(detect_vendor("Intel(R) UHD Graphics"), Some(GpuVendor::Intel));
        assert_eq!(detect_vendor("Microsoft Basic Display Adapter"), None);
    }

    #[test]
    fn urls_are_official_https() {
        for (v, host) in [
            (GpuVendor::Nvidia, "nvidia.com"),
            (GpuVendor::Amd, "amd.com"),
            (GpuVendor::Intel, "intel.com"),
        ] {
            let u = v.official_url();
            assert!(u.starts_with("https://"));
            assert!(u.contains(host));
        }
    }

    #[test]
    fn detects_display_class_only() {
        let g = InstalledDriver {
            device_name: "NVIDIA GeForce".into(),
            device_class: "DISPLAY".into(),
            provider: "NVIDIA".into(),
            version: "1.0".into(),
            ..Default::default()
        };
        let n = InstalledDriver {
            device_name: "NVIDIA HD Audio".into(),
            device_class: "MEDIA".into(),
            provider: "NVIDIA".into(),
            ..Default::default()
        };
        assert_eq!(detect_gpus(&[g, n]).len(), 1);
    }
}
