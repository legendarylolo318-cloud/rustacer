//! Live system metrics (CPU, RAM, swap, disks, network) and static system info.

use serde::{Deserialize, Serialize};
use std::time::Instant;
use sysinfo::{Disks, MemoryRefreshKind, Networks, ProcessesToUpdate, System};

#[derive(Debug, Clone, Default)]
pub struct DiskUsage {
    pub mount: String,
    pub name: String,
    pub total: u64,
    pub available: u64,
}

impl DiskUsage {
    pub fn used_percent(&self) -> f32 {
        percent(self.total.saturating_sub(self.available), self.total)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub cpu_total: f32,
    pub cpu_cores: Vec<f32>,
    pub ram_used: u64,
    pub ram_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disks: Vec<DiskUsage>,
    pub net_down_bps: f64,
    pub net_up_bps: f64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub uptime_secs: u64,
}

impl Snapshot {
    pub fn ram_percent(&self) -> f32 {
        percent(self.ram_used, self.ram_total)
    }
    pub fn swap_percent(&self) -> f32 {
        percent(self.swap_used, self.swap_total)
    }
}

pub fn percent(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64 * 100.0) as f32
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: String,
    pub os_build: String,
    pub hostname: String,
    pub cpu_model: String,
    pub physical_cores: usize,
    pub logical_cores: usize,
    pub gpus: Vec<String>,
    pub total_ram: u64,
    pub boot_time_unix: u64,
}

/// Collect static info. Queries WMI for the GPU, so call from a worker thread.
pub fn system_info() -> SystemInfo {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    SystemInfo {
        os: System::long_os_version().unwrap_or_else(|| "Windows".into()),
        os_build: System::kernel_version().unwrap_or_default(),
        hostname: System::host_name().unwrap_or_default(),
        cpu_model: sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_default(),
        physical_cores: System::physical_core_count().unwrap_or(0),
        logical_cores: sys.cpus().len(),
        gpus: gpu_names(),
        total_ram: sys.total_memory(),
        boot_time_unix: System::boot_time(),
    }
}

/// Fixed and removable volumes with their sizes (no live counters).
pub fn disk_usages() -> Vec<DiskUsage> {
    Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| DiskUsage {
            mount: d.mount_point().to_string_lossy().into_owned(),
            name: d.name().to_string_lossy().into_owned(),
            total: d.total_space(),
            available: d.available_space(),
        })
        .collect()
}

pub fn gpu_names() -> Vec<String> {
    #[derive(Deserialize)]
    #[serde(rename = "Win32_VideoController")]
    #[serde(rename_all = "PascalCase")]
    struct Vc {
        name: Option<String>,
    }
    let run = || -> Result<Vec<String>, crate::CoreError> {
        let _com = crate::util::ComGuard::new()?;
        let con = wmi::WMIConnection::new()?;
        let v: Vec<Vc> = con.query()?;
        Ok(v.into_iter().filter_map(|x| x.name).collect())
    };
    run().unwrap_or_default()
}

/// Stateful sampler; call `sample` once per refresh interval.
pub struct Sampler {
    sys: System,
    disks: Disks,
    nets: Networks,
    last: Instant,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            nets: Networks::new_with_refreshed_list(),
            last: Instant::now(),
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let dt = self.last.elapsed().as_secs_f64().max(0.001);
        self.last = Instant::now();
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        self.disks.refresh(true);
        self.nets.refresh(true);

        let (mut rx, mut tx) = (0u64, 0u64);
        for n in self.nets.values() {
            rx += n.received();
            tx += n.transmitted();
        }
        let (mut dr, mut dw) = (0u64, 0u64);
        for p in self.sys.processes().values() {
            let u = p.disk_usage();
            dr += u.read_bytes;
            dw += u.written_bytes;
        }
        Snapshot {
            cpu_total: self.sys.global_cpu_usage(),
            cpu_cores: self.sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
            ram_used: self.sys.used_memory(),
            ram_total: self.sys.total_memory(),
            swap_used: self.sys.used_swap(),
            swap_total: self.sys.total_swap(),
            disks: self
                .disks
                .iter()
                .filter(|d| d.total_space() > 0)
                .map(|d| DiskUsage {
                    mount: d.mount_point().to_string_lossy().into_owned(),
                    name: d.name().to_string_lossy().into_owned(),
                    total: d.total_space(),
                    available: d.available_space(),
                })
                .collect(),
            net_down_bps: rx as f64 / dt,
            net_up_bps: tx as f64 / dt,
            disk_read_bps: dr as f64 / dt,
            disk_write_bps: dw as f64 / dt,
            uptime_secs: System::uptime(),
        }
    }
}

pub fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = secs % 86_400 / 3600;
    let m = secs % 3600 / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else {
        format!("{h}h {m}m")
    }
}

/// Which alert thresholds are exceeded for a snapshot, as (stable key, message).
/// The key is used for the cooldown, so a value moving from 93% to 94% is the same alert.
pub fn exceeded_alerts(s: &Snapshot, cpu: f32, ram: f32, disk: f32) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if s.cpu_total >= cpu {
        v.push(("cpu".into(), format!("CPU usage is at {:.0}%", s.cpu_total)));
    }
    if s.ram_percent() >= ram {
        v.push((
            "ram".into(),
            format!("Memory usage is at {:.0}%", s.ram_percent()),
        ));
    }
    for d in &s.disks {
        if d.used_percent() >= disk {
            v.push((
                format!("disk:{}", d.mount),
                format!("Drive {} is {:.0}% full", d.mount, d.used_percent()),
            ));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_edge_cases() {
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(percent(50, 200), 25.0);
    }

    #[test]
    fn uptime_format() {
        assert_eq!(format_uptime(59), "0h 0m");
        assert_eq!(format_uptime(90_061), "1d 1h 1m");
    }

    #[test]
    fn alerts() {
        let s = Snapshot {
            cpu_total: 95.0,
            ram_used: 50,
            ram_total: 100,
            disks: vec![DiskUsage {
                mount: "C:\\".into(),
                name: String::new(),
                total: 100,
                available: 2,
            }],
            ..Default::default()
        };
        let a = exceeded_alerts(&s, 90.0, 90.0, 95.0);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].0, "cpu");
        let mut s2 = s.clone();
        s2.cpu_total = 96.0;
        assert_eq!(
            exceeded_alerts(&s2, 90.0, 90.0, 95.0)[0].0,
            a[0].0,
            "key is stable"
        );
        assert!(exceeded_alerts(&s, 99.0, 99.0, 99.0).is_empty());
    }
}
