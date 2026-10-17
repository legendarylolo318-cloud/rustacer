//! Rustacer core: all system logic, no GUI.

pub mod admin;
pub mod bootperf;
pub mod cleaner;
pub mod config;
pub mod diskusage;
pub mod drivehealth;
pub mod drivers;
pub mod error;
pub mod metrics;
pub mod netconn;
pub mod netinfo;
pub mod processes;
pub mod report;
pub mod services;
pub mod startup;
pub mod strings;
pub mod tray;
pub mod uninstaller;
pub mod util;

pub use error::{CoreError, Result};
