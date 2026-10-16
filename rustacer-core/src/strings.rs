//! Language-ready string table. Only English for now; add a new `Lang`
//! variant and a matching arm to localise.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    English,
}

/// Look up a UI string by key. Unknown keys are returned unchanged so a
/// missing translation is visible rather than fatal.
pub fn tr(lang: Lang, key: &'static str) -> &'static str {
    match lang {
        Lang::English => en(key),
    }
}

fn en(key: &'static str) -> &'static str {
    match key {
        "nav.dashboard" => "Dashboard",
        "nav.startup" => "Startup Apps",
        "nav.cleaner" => "System Cleaner",
        "nav.drivers" => "Driver Updater",
        "nav.services" => "Services",
        "nav.processes" => "Processes",
        "nav.uninstaller" => "Uninstaller",
        "nav.resources" => "Resources",
        "nav.settings" => "Settings",
        "admin.restart" => "Restart as admin",
        "admin.needed" => "This feature needs administrator rights.",
        "common.search" => "Search",
        "common.refresh" => "Refresh",
        "common.cancel" => "Cancel",
        "common.confirm" => "Confirm",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_and_fallback() {
        assert_eq!(tr(Lang::English, "nav.dashboard"), "Dashboard");
        assert_eq!(tr(Lang::English, "nope"), "nope");
    }
}
