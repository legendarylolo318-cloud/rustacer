use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Midnight,
    Nord,
    Dracula,
    Solarized,
    Forest,
    HighContrast,
}

impl Theme {
    pub const ALL: [Theme; 8] = [
        Theme::Dark,
        Theme::Light,
        Theme::Midnight,
        Theme::Nord,
        Theme::Dracula,
        Theme::Solarized,
        Theme::Forest,
        Theme::HighContrast,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::Midnight => "Midnight",
            Theme::Nord => "Nord",
            Theme::Dracula => "Dracula",
            Theme::Solarized => "Solarized Dark",
            Theme::Forest => "Forest",
            Theme::HighContrast => "High contrast",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub theme: Theme,
    pub refresh_interval_ms: u64,
    pub cpu_alert_percent: f32,
    pub ram_alert_percent: f32,
    pub disk_alert_percent: f32,
    pub alerts_enabled: bool,
    pub old_driver_years: u32,
    pub check_drivers_on_startup: bool,
    pub start_with_windows: bool,
    pub hide_inbox_drivers: bool,
    /// UI scale (text and controls), 0.75 ..= 2.0.
    pub ui_scale: f32,
    /// Turn off animations.
    pub reduce_motion: bool,
    /// Keep running in the notification area when the window is closed. Off by default.
    pub close_to_tray: bool,
    /// Start hidden in the notification area when launched by "Start with Windows".
    /// Only meaningful while `start_with_windows` is on. Off by default.
    pub start_minimized_to_tray: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            refresh_interval_ms: 1000,
            cpu_alert_percent: 90.0,
            ram_alert_percent: 90.0,
            disk_alert_percent: 95.0,
            alerts_enabled: true,
            old_driver_years: 3,
            check_drivers_on_startup: false,
            start_with_windows: false,
            hide_inbox_drivers: true,
            ui_scale: 1.0,
            reduce_motion: false,
            close_to_tray: false,
            start_minimized_to_tray: false,
        }
    }
}

impl Config {
    /// Clamp values into sane ranges (guards against hand-edited files).
    pub fn sanitized(mut self) -> Self {
        self.refresh_interval_ms = self.refresh_interval_ms.clamp(250, 10_000);
        self.cpu_alert_percent = self.cpu_alert_percent.clamp(1.0, 100.0);
        self.ram_alert_percent = self.ram_alert_percent.clamp(1.0, 100.0);
        self.disk_alert_percent = self.disk_alert_percent.clamp(1.0, 100.0);
        self.old_driver_years = self.old_driver_years.clamp(1, 30);
        self.ui_scale = if self.ui_scale.is_finite() {
            self.ui_scale.clamp(0.75, 2.0)
        } else {
            1.0
        };
        // Starting minimized only makes sense when Windows starts Rustacer.
        if !self.start_with_windows {
            self.start_minimized_to_tray = false;
        }
        self
    }

    /// Is a tray icon wanted at all?
    pub fn tray_enabled(&self) -> bool {
        self.close_to_tray || self.start_minimized_to_tray
    }

    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str::<Config>(s)
            .map(Config::sanitized)
            .map_err(|e| CoreError::Config(e.to_string()))
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| CoreError::Config(e.to_string()))
    }

    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("Rustacer").join("config.toml"))
    }

    /// Load the config; any failure yields defaults (logged by the caller).
    pub fn load() -> Self {
        let Some(p) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&p) {
            Ok(s) => match Self::from_toml(&s) {
                Ok(c) => c,
                Err(e) => {
                    // Keep the broken file for the user instead of silently overwriting it later.
                    let bad = p.with_extension("toml.bad");
                    let _ = std::fs::rename(&p, &bad);
                    tracing::warn!("config was invalid ({e}); moved to {}", bad.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path().ok_or_else(|| CoreError::Config("no %APPDATA% directory".into()))?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, self.to_toml()?)?;
        Ok(())
    }
}

/// %LOCALAPPDATA%\Rustacer\logs
pub fn log_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Rustacer").join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let c = Config::default();
        let s = c.to_toml().unwrap();
        assert_eq!(Config::from_toml(&s).unwrap(), c);
    }

    #[test]
    fn partial_file_uses_defaults_and_clamps() {
        let c = Config::from_toml("refresh_interval_ms = 1\nold_driver_years = 999").unwrap();
        assert_eq!(c.refresh_interval_ms, 250);
        assert_eq!(c.old_driver_years, 30);
        assert_eq!(c.cpu_alert_percent, 90.0);
    }

    #[test]
    fn every_theme_roundtrips() {
        for t in Theme::ALL {
            let c = Config {
                theme: t,
                ..Config::default()
            };
            assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap().theme, t);
        }
        assert!(Config::from_toml("theme = 'not-a-theme'").is_err());
    }

    #[test]
    fn accessibility_settings_clamp() {
        let c = Config::from_toml("ui_scale = 9.0\nreduce_motion = true\ntheme = \"highcontrast\"")
            .unwrap();
        assert_eq!(c.ui_scale, 2.0);
        assert!(c.reduce_motion);
        assert_eq!(c.theme, Theme::HighContrast);
        assert_eq!(Config::from_toml("ui_scale = 0.1").unwrap().ui_scale, 0.75);
    }

    #[test]
    fn tray_is_off_by_default() {
        let c = Config::default();
        assert!(!c.close_to_tray && !c.start_minimized_to_tray && !c.tray_enabled());
        // An old config file without the new keys keeps the tray off.
        let old = Config::from_toml("theme = \"nord\"").unwrap();
        assert!(!old.tray_enabled());
    }

    #[test]
    fn start_minimized_requires_start_with_windows() {
        let c = Config::from_toml("start_minimized_to_tray = true\nclose_to_tray = true").unwrap();
        assert!(
            !c.start_minimized_to_tray,
            "no start-with-Windows, no start-minimized"
        );
        assert!(c.close_to_tray && c.tray_enabled());
        let c =
            Config::from_toml("start_minimized_to_tray = true\nstart_with_windows = true").unwrap();
        assert!(c.start_minimized_to_tray && c.tray_enabled());
        // Round trip keeps the flags.
        assert_eq!(Config::from_toml(&c.to_toml().unwrap()).unwrap(), c);
    }

    #[test]
    fn garbage_is_error() {
        assert!(Config::from_toml("= = =").is_err());
    }
}
