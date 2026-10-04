//! Navigation model shared by the sidebar, the mobile drawer and the command palette.

#[derive(Clone, Copy, Debug)]
pub struct NavItem {
    pub to: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
}

pub const PRIMARY: &[NavItem] = &[
    NavItem { to: "/", label: "Home", icon: "home" },
    NavItem { to: "/tracks", label: "Tracks", icon: "list-music" },
    NavItem { to: "/albums", label: "Albums", icon: "disc" },
    NavItem { to: "/artists", label: "Artists", icon: "user" },
    NavItem { to: "/labels", label: "Labels", icon: "folder" },
];

pub const MORE: &[(&str, &[NavItem])] = &[
    (
        "Collection",
        &[
            NavItem { to: "/loved", label: "Loved", icon: "heart" },
            NavItem { to: "/tags", label: "Tags", icon: "tag" },
            NavItem { to: "/playlists", label: "Playlists", icon: "list" },
            NavItem { to: "/sets", label: "DJ Sets", icon: "sliders" },
        ],
    ),
    (
        "Discover",
        &[
            NavItem { to: "/explore", label: "Explore", icon: "compass" },
            NavItem { to: "/feed", label: "Feed", icon: "rss" },
            NavItem { to: "/harvest", label: "Harvest", icon: "sparkles" },
            NavItem { to: "/fans", label: "Fans", icon: "users" },
        ],
    ),
    (
        "Manage",
        &[
            NavItem { to: "/downloads", label: "Downloads", icon: "download" },
            NavItem { to: "/tracklists", label: "Tracklists", icon: "list-music" },
            NavItem { to: "/analysis", label: "Analysis", icon: "activity" },
            NavItem { to: "/cleanup", label: "Cleanup", icon: "broom" },
        ],
    ),
];

/// The bottom tab bar on phones: the first primary routes; everything else sits behind "More".
pub const TABS: &[NavItem] = PRIMARY.split_at(4).0;

pub const SETTINGS: NavItem = NavItem { to: "/settings", label: "Settings", icon: "settings" };

pub fn all() -> Vec<NavItem> {
    let mut v: Vec<NavItem> = PRIMARY.to_vec();
    for (_, items) in MORE {
        v.extend_from_slice(items);
    }
    v.push(SETTINGS);
    v
}

/// Whether `to` is the active route for `path`.
pub fn is_active(to: &str, path: &str) -> bool {
    if to == "/" {
        return path == "/";
    }
    path == to || path.starts_with(&format!("{to}/"))
}

/// Whether `path` belongs to one of the tab bar's routes (otherwise "More" is the active tab).
pub fn in_tabs(path: &str) -> bool {
    TABS.iter().any(|t| is_active(t.to, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_matching() {
        assert!(is_active("/", "/"));
        assert!(!is_active("/", "/tracks"));
        assert!(is_active("/albums", "/albums/12"));
        assert!(!is_active("/albums", "/albums-x"));
        assert!(is_active("/explore", "/explore/band"));
    }
    #[test]
    fn every_nav_target_is_unique() {
        let mut v: Vec<_> = all().iter().map(|n| n.to).collect();
        v.sort();
        let n = v.len();
        v.dedup();
        assert_eq!(n, v.len());
    }
    #[test]
    fn tabs_cover_primary_routes_only() {
        assert_eq!(TABS.len(), 4);
        assert!(TABS.iter().all(|t| PRIMARY.iter().any(|p| p.to == t.to)));
        assert!(in_tabs("/"));
        assert!(in_tabs("/albums/12"));
        assert!(!in_tabs("/loved"));
        assert!(!in_tabs("/settings"));
        assert!(!in_tabs("/labels"));
    }
}
