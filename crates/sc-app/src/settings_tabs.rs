//! **Settings tabs**: how an application's settings are laid out, declared by its
//! framework (TODO UX2.1).
//!
//! An application record has a dozen parts — its identity, its framework and the
//! framework's own settings, its tables, triggers, APIs, roles, content-security
//! policy — and one screen holding all of them is a screen nobody can find
//! anything on. So the admin UI shows them in tabs, and **the framework says
//! which tabs**: a React app's settings are about its data and its API, a
//! Saltcorn UI app's are about a site with views, pages and sign-ups, and one
//! arrangement fits neither.
//!
//! The vocabulary is a fixed set of **groups**, each one part of the form (or a
//! screen of the application's, such as its views) with an id that never
//! changes. A layout is a list of [`SettingsTab`]s, each naming its groups in
//! order. A framework's own settings are placed by [`FormField::tab`]; the
//! [`GROUP_SETTINGS`] entry marks where in a tab they appear.
//!
//! **Nothing can be lost.** A layout is a presentation, so it may rearrange the
//! form but not remove part of it: [`resolve_settings_tabs`] puts every group a
//! layout leaves out on a tab, and a layout that is wrong — a group twice, an id
//! it does not know — is replaced by the default one with the reason
//! ([`settings_tabs_problems`]), which a module loader reports on the module.
//!
//! [`FormField::tab`]: sc_types::FormField::tab

use serde::{Deserialize, Serialize};

/// One tab of an application's settings: its id (what the admin UI's route
/// names), its label, and the groups it holds, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsTab {
    /// `data`, `security` — letters, digits, `-` and `_`.
    pub id: String,
    /// What the tab says: `Users & Security`.
    pub label: String,
    /// The groups on the tab, in order: [`RECORD_GROUPS`], [`SCREEN_GROUPS`]
    /// and [`GROUP_SETTINGS`].
    #[serde(default)]
    pub groups: Vec<String>,
}

impl SettingsTab {
    /// A tab holding `groups`.
    pub fn new(id: &str, label: &str, groups: &[&str]) -> SettingsTab {
        SettingsTab {
            id: id.to_owned(),
            label: label.to_owned(),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
        }
    }
}

/// The application's name, subdomain and description.
pub const GROUP_IDENTITY: &str = "identity";
/// The framework choice.
pub const GROUP_FRAMEWORK: &str = "framework";
/// Where the framework's own settings for this tab go — those whose
/// [`tab`](sc_types::FormField::tab) is this tab's id, or none on the tab
/// holding [`GROUP_FRAMEWORK`].
pub const GROUP_SETTINGS: &str = "settings";
/// Each build target's own settings (an APK's signing).
pub const GROUP_TARGETS: &str = "targets";
/// The tables the application may use.
pub const GROUP_TABLES: &str = "tables";
/// The file stores the application may use.
pub const GROUP_FILE_STORES: &str = "file_stores";
/// The triggers the application may run.
pub const GROUP_TRIGGERS: &str = "triggers";
/// The streams the application subscribes to.
pub const GROUP_STREAMS: &str = "streams";
/// The API providers it mounts.
pub const GROUP_APIS: &str = "apis";
/// The tables with a row API.
pub const GROUP_ROW_API: &str = "row_api";
/// Its static directories.
pub const GROUP_STATIC_DIRS: &str = "static_dirs";
/// Its upload slots.
pub const GROUP_UPLOADS: &str = "uploads";
/// The agents it exposes.
pub const GROUP_AGENTS: &str = "agents";
/// Who may use it: the role range and the access condition.
pub const GROUP_ACCESS: &str = "access";
/// Guest sessions.
pub const GROUP_GUEST_SESSIONS: &str = "guest_sessions";
/// Its content-security policy and the modules' suggestions for it.
pub const GROUP_CSP: &str = "csp";
/// Which coding agent the sidebar opens.
pub const GROUP_CODING_AGENT: &str = "coding_agent";
/// Whether its checklists run after each build.
pub const GROUP_CHECKLISTS: &str = "checklists";
/// The Views screen.
pub const GROUP_VIEWS: &str = "views";
/// The Pages screen.
pub const GROUP_PAGES: &str = "pages";
/// The Library screen.
pub const GROUP_LIBRARY: &str = "library";
/// The Translations editor.
pub const GROUP_TRANSLATIONS: &str = "translations";

/// The parts of the application record, each of which a resolved layout puts
/// on exactly one tab.
pub const RECORD_GROUPS: [&str; 17] = [
    GROUP_IDENTITY,
    GROUP_FRAMEWORK,
    GROUP_TARGETS,
    GROUP_TABLES,
    GROUP_FILE_STORES,
    GROUP_TRIGGERS,
    GROUP_STREAMS,
    GROUP_APIS,
    GROUP_ROW_API,
    GROUP_STATIC_DIRS,
    GROUP_UPLOADS,
    GROUP_AGENTS,
    GROUP_ACCESS,
    GROUP_GUEST_SESSIONS,
    GROUP_CSP,
    GROUP_CODING_AGENT,
    GROUP_CHECKLISTS,
];

/// The application's screens. Views, Pages and Library exist only for a
/// framework with views, and each has a tab to itself, because each is a list
/// screen of its own; the Translations editor may share a tab.
pub const SCREEN_GROUPS: [&str; 4] = [GROUP_VIEWS, GROUP_PAGES, GROUP_LIBRARY, GROUP_TRANSLATIONS];

/// The screens only an application with views has, each alone on its tab.
pub const VIEW_SCREENS: [&str; 3] = [GROUP_VIEWS, GROUP_PAGES, GROUP_LIBRARY];

/// The layout every framework gets unless it declares one: Framework, Data,
/// Triggers & Streams, API, Security, Development, Translations — with a
/// framework that has views getting Views, Pages and Library after Framework.
pub fn default_settings_tabs(has_views: bool) -> Vec<SettingsTab> {
    let mut tabs = vec![SettingsTab::new(
        "framework",
        "Framework",
        &[
            GROUP_IDENTITY,
            GROUP_FRAMEWORK,
            GROUP_SETTINGS,
            GROUP_TARGETS,
        ],
    )];
    if has_views {
        tabs.extend(view_screen_tabs(&VIEW_SCREENS));
    }
    tabs.extend([
        SettingsTab::new("data", "Data", &[GROUP_TABLES, GROUP_FILE_STORES]),
        SettingsTab::new(
            "triggers",
            "Triggers & Streams",
            &[GROUP_TRIGGERS, GROUP_STREAMS],
        ),
        SettingsTab::new(
            "api",
            "API",
            &[
                GROUP_APIS,
                GROUP_ROW_API,
                GROUP_AGENTS,
                GROUP_UPLOADS,
                GROUP_STATIC_DIRS,
            ],
        ),
        SettingsTab::new(
            "security",
            "Security",
            &[GROUP_ACCESS, GROUP_GUEST_SESSIONS, GROUP_CSP],
        ),
        SettingsTab::new(
            "development",
            "Development",
            &[GROUP_CODING_AGENT, GROUP_CHECKLISTS],
        ),
        SettingsTab::new("translations", "Translations", &[GROUP_TRANSLATIONS]),
    ]);
    tabs
}

/// A tab of its own for each of `screens`.
fn view_screen_tabs(screens: &[&str]) -> Vec<SettingsTab> {
    screens
        .iter()
        .map(|g| {
            let label = match *g {
                GROUP_VIEWS => "Views",
                GROUP_PAGES => "Pages",
                _ => "Library",
            };
            SettingsTab::new(g, label, &[g])
        })
        .collect()
}

/// Why `tabs` is not a usable layout for a framework (with views or without),
/// one sentence each; empty when it is.
///
/// Leaving a group out is **not** a problem — [`resolve_settings_tabs`] places
/// it — so a framework can declare only the tabs it cares about.
pub fn settings_tabs_problems(tabs: &[SettingsTab], has_views: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if tabs.is_empty() {
        problems.push("it declares no tabs".to_owned());
        return problems;
    }
    let mut ids: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    for tab in tabs {
        let id = tab.id.as_str();
        if !valid_tab_id(id) {
            problems.push(format!(
                "the tab id {id:?} is not letters, digits, `-` and `_` starting with a letter"
            ));
        } else if ids.contains(&id) {
            problems.push(format!("the tab id `{id}` is used twice"));
        }
        ids.push(id);
        if tab.label.trim().is_empty() {
            problems.push(format!("the tab `{id}` has no label"));
        }
        for group in &tab.groups {
            let group = group.as_str();
            let known = group == GROUP_SETTINGS
                || RECORD_GROUPS.contains(&group)
                || SCREEN_GROUPS.contains(&group);
            if !known {
                problems.push(format!(
                    "the tab `{id}` names the group `{group}`, which does not exist"
                ));
                continue;
            }
            if group != GROUP_SETTINGS {
                if placed.contains(&group) {
                    problems.push(format!("the group `{group}` is on two tabs"));
                }
                placed.push(group);
            }
            if VIEW_SCREENS.contains(&group) {
                if !has_views {
                    problems.push(format!(
                        "the tab `{id}` holds `{group}`, which only a framework with views has"
                    ));
                } else if tab.groups.len() > 1 {
                    problems.push(format!(
                        "the tab `{id}` holds `{group}` with other groups; it needs a tab of its own"
                    ));
                }
            }
        }
    }
    // A new application is created from its first tab alone, so that tab has to
    // be where it is named and its framework chosen.
    for group in [GROUP_IDENTITY, GROUP_FRAMEWORK] {
        if !tabs[0].groups.iter().any(|g| g == group) {
            problems.push(format!("its first tab does not hold `{group}`"));
        }
    }
    problems
}

/// Whether `id` can name a tab: it goes into the admin UI's route.
fn valid_tab_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The layout an application's form is shown with: `declared` when it is
/// usable ([`settings_tabs_problems`]), else the [default](default_settings_tabs),
/// **completed** so every part of the form is on a tab.
///
/// What completing does: a view screen the layout left out gets a tab of its
/// own after the first; every other group left out goes on the last tab that is
/// not a view screen; and the tab holding [`GROUP_FRAMEWORK`] gets a
/// [`GROUP_SETTINGS`] entry after it if it has none, because that is where a
/// framework setting naming no tab is shown.
pub fn resolve_settings_tabs(
    declared: Option<&[SettingsTab]>,
    has_views: bool,
) -> Vec<SettingsTab> {
    let mut tabs = match declared {
        Some(tabs) if settings_tabs_problems(tabs, has_views).is_empty() => tabs.to_vec(),
        _ => default_settings_tabs(has_views),
    };
    let placed = |tabs: &[SettingsTab], group: &str| {
        tabs.iter().any(|t| t.groups.iter().any(|g| g == group))
    };

    if has_views {
        let missing: Vec<&str> = VIEW_SCREENS
            .iter()
            .copied()
            .filter(|g| !placed(&tabs, g))
            .collect();
        let at = 1.min(tabs.len());
        tabs.splice(at..at, view_screen_tabs(&missing));
    }

    let missing: Vec<String> = RECORD_GROUPS
        .iter()
        .chain([GROUP_TRANSLATIONS].iter())
        .copied()
        .filter(|g| !placed(&tabs, g))
        .map(str::to_owned)
        .collect();
    if !missing.is_empty() {
        // The first tab is never a view screen (the first tab holds the
        // framework choice), so there always is one.
        let last = tabs
            .iter()
            .rposition(|t| !t.groups.iter().any(|g| VIEW_SCREENS.contains(&g.as_str())))
            .unwrap_or(0);
        tabs[last].groups.extend(missing);
    }

    if let Some(tab) = tabs
        .iter_mut()
        .find(|t| t.groups.iter().any(|g| g == GROUP_FRAMEWORK))
        && !tab.groups.iter().any(|g| g == GROUP_SETTINGS)
    {
        let at = tab
            .groups
            .iter()
            .position(|g| g == GROUP_FRAMEWORK)
            .map_or(0, |i| i + 1);
        tab.groups.insert(at, GROUP_SETTINGS.to_owned());
    }
    tabs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every record group and the translations editor once, the view screens
    /// once each for a framework with views, and nothing else twice.
    fn assert_complete(tabs: &[SettingsTab], has_views: bool) {
        let all: Vec<&str> = tabs
            .iter()
            .flat_map(|t| t.groups.iter().map(String::as_str))
            .filter(|g| *g != GROUP_SETTINGS)
            .collect();
        let mut expected: Vec<&str> = RECORD_GROUPS.to_vec();
        expected.push(GROUP_TRANSLATIONS);
        if has_views {
            expected.extend(VIEW_SCREENS);
        }
        let mut sorted = all.clone();
        sorted.sort_unstable();
        expected.sort_unstable();
        assert_eq!(sorted, expected, "{tabs:#?}");
    }

    #[test]
    fn the_default_layout_is_the_six_tabs_and_translations() {
        let tabs = default_settings_tabs(false);
        let labels: Vec<&str> = tabs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Framework",
                "Data",
                "Triggers & Streams",
                "API",
                "Security",
                "Development",
                "Translations"
            ]
        );
        assert!(settings_tabs_problems(&tabs, false).is_empty());
        assert_eq!(resolve_settings_tabs(None, false), tabs);
        assert_complete(&tabs, false);

        // With views, the three screens follow the first tab.
        let tabs = resolve_settings_tabs(None, true);
        let ids: Vec<&str> = tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(&ids[..4], ["framework", "views", "pages", "library"]);
        assert_complete(&tabs, true);
    }

    #[test]
    fn a_group_left_out_lands_on_the_last_tab() {
        let declared = vec![
            SettingsTab::new("site", "Site", &[GROUP_IDENTITY, GROUP_FRAMEWORK]),
            SettingsTab::new("views", "Views", &[GROUP_VIEWS]),
            SettingsTab::new("more", "More", &[GROUP_TABLES]),
            SettingsTab::new("pages", "Pages", &[GROUP_PAGES]),
        ];
        assert!(settings_tabs_problems(&declared, true).is_empty());
        let tabs = resolve_settings_tabs(Some(&declared), true);
        assert_complete(&tabs, true);
        // Library, left out, gets its own tab after the first...
        assert_eq!(tabs[1].id, "library");
        // ...the framework tab gets the place its settings go...
        assert_eq!(
            tabs[0].groups,
            [GROUP_IDENTITY, GROUP_FRAMEWORK, GROUP_SETTINGS]
        );
        // ...and the rest go on the last tab that is not a view screen.
        let more = tabs.iter().find(|t| t.id == "more").unwrap();
        assert!(more.groups.iter().any(|g| g == GROUP_CSP), "{more:?}");
        assert!(
            more.groups.iter().any(|g| g == GROUP_TRANSLATIONS),
            "{more:?}"
        );
        assert_eq!(tabs.last().unwrap().groups, [GROUP_PAGES]);
    }

    #[test]
    fn a_bad_layout_says_why_and_the_default_is_used() {
        let declared = vec![
            SettingsTab::new("data", "Data", &[GROUP_TABLES, "colours"]),
            SettingsTab::new("data", "", &[GROUP_TABLES, GROUP_VIEWS]),
            SettingsTab::new("9lives", "Odd", &[]),
        ];
        let problems = settings_tabs_problems(&declared, false).join("\n");
        for expected in [
            "`colours`, which does not exist",
            "`data` is used twice",
            "the tab `data` has no label",
            "`tables` is on two tabs",
            "only a framework with views has",
            "\"9lives\"",
            "does not hold `identity`",
            "does not hold `framework`",
        ] {
            assert!(problems.contains(expected), "{expected}: {problems}");
        }
        assert_eq!(
            resolve_settings_tabs(Some(&declared), false),
            default_settings_tabs(false)
        );

        // A view screen shares no tab.
        let shared = vec![SettingsTab::new(
            "site",
            "Site",
            &[GROUP_IDENTITY, GROUP_FRAMEWORK, GROUP_VIEWS],
        )];
        assert!(
            settings_tabs_problems(&shared, true)
                .iter()
                .any(|p| p.contains("a tab of its own"))
        );
        assert!(!settings_tabs_problems(&[], true).is_empty());
    }

    #[test]
    fn a_layout_reads_from_json_with_groups_optional() {
        let tabs: Vec<SettingsTab> = serde_json::from_value(serde_json::json!([
            { "id": "framework", "label": "Framework", "groups": ["identity", "framework"] },
            { "id": "mobile", "label": "Mobile" }
        ]))
        .unwrap();
        assert!(tabs[1].groups.is_empty());
        assert!(settings_tabs_problems(&tabs, false).is_empty());
        assert_complete(&resolve_settings_tabs(Some(&tabs), false), false);
    }
}
