//! Process listing and termination.

use crate::error::{CoreError, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use sysinfo::{Pid, ProcessesToUpdate, System, Users};
use tracing::info;

/// Processes that must never be terminated.
pub const CRITICAL: &[&str] = &[
    "csrss.exe",
    "wininit.exe",
    "smss.exe",
    "lsass.exe",
    "services.exe",
    "winlogon.exe",
    "system",
    "registry",
    "memory compression",
    "secure system",
    "system idle process",
    "dwm.exe",
    "fontdrvhost.exe",
];

pub fn is_critical(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    CRITICAL.contains(&l.as_str())
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cpu: f32,
    pub memory: u64,
    pub user: String,
    pub path: Option<PathBuf>,
}

pub struct ProcessMonitor {
    sys: System,
    users: Users,
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessMonitor {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        Self {
            sys,
            users: Users::new_with_refreshed_list(),
        }
    }

    pub fn refresh(&mut self) -> Vec<ProcessInfo> {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let cores = self.sys.cpus().len().max(1) as f32;
        let cores = if cores <= 1.0 {
            std::thread::available_parallelism().map(|n| n.get() as f32).unwrap_or(1.0)
        } else {
            cores
        };
        self.sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcessInfo {
                pid: pid.as_u32(),
                parent: p.parent().map(|x| x.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage() / cores,
                memory: p.memory(),
                user: p
                    .user_id()
                    .and_then(|u| self.users.get_user_by_id(u))
                    .map(|u| u.name().to_string())
                    .unwrap_or_default(),
                path: p.exe().map(|e| e.to_path_buf()),
            })
            .collect()
    }

    /// End a single process after the critical check.
    pub fn kill(&mut self, pid: u32) -> Result<()> {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let p = self
            .sys
            .process(Pid::from_u32(pid))
            .ok_or_else(|| CoreError::Message(format!("Process {pid} no longer exists.")))?;
        let name = p.name().to_string_lossy().into_owned();
        guard(pid, &name)?;
        if p.kill() {
            info!("killed process {name} ({pid})");
            Ok(())
        } else {
            Err(CoreError::Message(format!(
                "Could not end '{name}'. It may be protected or require administrator rights."
            )))
        }
    }

    /// End a process and all its descendants (children first).
    pub fn kill_tree(&mut self, pid: u32) -> Result<usize> {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let parents: HashMap<u32, Option<u32>> = self
            .sys
            .processes()
            .iter()
            .map(|(k, p)| (k.as_u32(), p.parent().map(|x| x.as_u32())))
            .collect();
        let names: HashMap<u32, String> = self
            .sys
            .processes()
            .iter()
            .map(|(k, p)| (k.as_u32(), p.name().to_string_lossy().into_owned()))
            .collect();
        let order = tree_order(&parents, pid);
        // Check every member first so nothing is killed if any is critical.
        for id in &order {
            if let Some(n) = names.get(id) {
                guard(*id, n)?;
            }
        }
        let mut killed = 0;
        for id in order {
            if let Some(p) = self.sys.process(Pid::from_u32(id)) {
                if p.kill() {
                    killed += 1;
                }
            }
        }
        info!("ended process tree rooted at {pid}: {killed} processes");
        Ok(killed)
    }
}

fn guard(pid: u32, name: &str) -> Result<()> {
    if pid <= 4 || is_critical(name) {
        Err(CoreError::Blocked(format!(
            "'{name}' is a critical system process and cannot be ended."
        )))
    } else if pid == std::process::id() {
        Err(CoreError::Blocked("Rustacer will not end itself from here.".into()))
    } else {
        Ok(())
    }
}

/// Descendants of `root` (deepest first), ending with `root` itself.
pub fn tree_order(parents: &HashMap<u32, Option<u32>>, root: u32) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i];
        for (child, par) in parents {
            if *par == Some(cur) && *child != cur && !out.contains(child) {
                out.push(*child);
            }
        }
        i += 1;
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_names() {
        assert!(is_critical("CSRSS.exe"));
        assert!(is_critical("System"));
        assert!(is_critical("lsass.exe"));
        assert!(!is_critical("notepad.exe"));
    }

    #[test]
    fn guard_blocks_low_pids_and_critical() {
        assert!(guard(4, "System").is_err());
        assert!(guard(1234, "winlogon.exe").is_err());
        assert!(guard(1234, "notepad.exe").is_ok());
    }

    #[test]
    fn tree_is_children_first() {
        let mut m = HashMap::new();
        m.insert(1, None);
        m.insert(2, Some(1));
        m.insert(3, Some(2));
        m.insert(4, Some(1));
        m.insert(9, Some(8));
        let o = tree_order(&m, 1);
        assert_eq!(*o.last().unwrap(), 1);
        assert_eq!(o.len(), 4);
        assert!(o.iter().position(|x| *x == 3) < o.iter().position(|x| *x == 2));
        assert!(!o.contains(&9));
    }
}
