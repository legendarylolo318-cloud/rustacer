//! Driver updater. Windows Update and official vendor links are the only sources.

pub mod backup;
pub mod gpu;
pub mod installed;
pub mod restore;
pub mod version;
pub mod wua;

pub use installed::InstalledDriver;
pub use wua::DriverUpdate;
