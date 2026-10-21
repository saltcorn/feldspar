//! The Analytics UI as an application framework (analytics TODO A9.2–A9.4).
//!
//! An admin publishes a restricted part of the Analytics UI to end users by
//! creating an application whose framework is `analytics`. Its settings say
//! which part, in one of two modes:
//!
//! - **Fixed**: a few workspaces the admin made — a dashboard, a report —
//!   shown and nothing else. No workspace list, no Dataset editor, nothing
//!   saved: what a person changes on the screen (a filter, a selection) is
//!   theirs until they leave. They read the datasets those workspaces read, and
//!   no other.
//! - **Self-serve**: a small Analytics UI of their own. The tables they may
//!   build datasets on are **the application's tables** — the subset every
//!   application already declares (§13.2), rather than a second list here —
//!   the Dataset editor is on or off, and the kinds of workspace they may open
//!   and whether they may create them are the admin's to say. What a person
//!   makes there is theirs, and shared with a role if they say so.
//!
//! Either way the application has a **role floor**: below it, the Analytics UI
//! answers nothing but "sign in".
//!
//! This module is the policy, as data and checks: [`AnalyticsConfig`] reads and
//! checks the settings, and [`AppScope`] answers, for one request, what it may
//! reach. The server's handlers ask it before they do anything, and its tables
//! go into the [`Caller`] every read is guarded with (`sc_dataset::Reader`), so
//! a table outside them is refused in the statement whatever the client sent.
//! Under all of it the caller's own authority still holds (A9.1): an
//! application decides what may be asked for, and the table permissions and
//! ownership formulas decide which rows come back.

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::Catalog;
use sc_dataset::{Base, Caller, DatasetDef, DatasetId, Library, Sharing};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde::Serialize;
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::panel::workspace_datasets;
use crate::workspace::{Workspace, WorkspaceId, WorkspaceKind, list_workspaces, load_workspace};

/// The registered name of the framework.
pub const ANALYTICS_FRAMEWORK: &str = "analytics";

/// The `mode` setting: `fixed` or `self_serve`.
pub const CFG_MODE: &str = "mode";
/// The `min_role` setting: the least privileged role that may use it.
pub const CFG_MIN_ROLE: &str = "min_role";
/// The `workspaces` setting (fixed mode): the workspaces shown, by name or id.
pub const CFG_WORKSPACES: &str = "workspaces";
/// The `dataset_editor` setting (self-serve mode).
pub const CFG_DATASET_EDITOR: &str = "dataset_editor";
/// The `workspace_kinds` setting (self-serve mode): the kinds that may be opened.
pub const CFG_WORKSPACE_KINDS: &str = "workspace_kinds";
/// The `create_workspaces` setting (self-serve mode).
pub const CFG_CREATE_WORKSPACES: &str = "create_workspaces";

/// What an application shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The workspaces it names, and nothing else.
    Fixed,
    /// A restricted Analytics UI of the user's own.
    SelfServe,
}

impl Mode {
    /// Its name as the setting holds it.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Fixed => "fixed",
            Mode::SelfServe => "self_serve",
        }
    }
}

/// The settings of an Analytics application, in the one [`FormField`]
/// vocabulary every framework uses: the admin UI renders them without knowing
/// this framework, and shows each mode's only while that mode is chosen.
///
/// The role floor defaults to the admin's, so an application nobody has
/// thought about yet is shown to nobody else.
pub fn analytics_config_spec() -> Vec<FormField> {
    let fixed = || vec![json!(Mode::Fixed.as_str())];
    let self_serve = || vec![json!(Mode::SelfServe.as_str())];
    vec![
        FormField::new(CFG_MODE, BasicType::Text)
            .label("What it shows")
            .required()
            .options([Mode::Fixed.as_str(), Mode::SelfServe.as_str()])
            .default_value(Mode::Fixed.as_str()),
        FormField::new(CFG_MIN_ROLE, BasicType::Int)
            .label("Least privileged role that may use it (1 is the admin, 100 everybody)")
            .required()
            .default_value(1),
        FormField::new(CFG_WORKSPACES, BasicType::Json)
            .label("Workspaces shown, by name or id, like [\"Incidents dashboard\"]")
            .default_value(Json::Array(Vec::new()))
            .show_if(CFG_MODE, fixed()),
        FormField::new(CFG_DATASET_EDITOR, BasicType::Bool)
            .label("Users may create and edit datasets on the application's tables")
            .default_value(false)
            .show_if(CFG_MODE, self_serve()),
        FormField::new(CFG_WORKSPACE_KINDS, BasicType::Json)
            .label(
                "Kinds of workspace users may open, like [\"data_explorer\", \"dashboard\"] \
                 (data_explorer, report, map, dashboard)",
            )
            .default_value(json!([WorkspaceKind::DataExplorer.as_str()]))
            .show_if(CFG_MODE, self_serve()),
        FormField::new(CFG_CREATE_WORKSPACES, BasicType::Bool)
            .label("Users may create workspaces of those kinds")
            .default_value(true)
            .show_if(CFG_MODE, self_serve()),
    ]
}

/// An Analytics application's settings, read and checked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnalyticsConfig {
    /// What it shows.
    pub mode: Mode,
    /// The least privileged role that may use it.
    pub min_role: u8,
    /// Fixed mode: the workspaces shown, by name or id, in order.
    pub workspaces: Vec<String>,
    /// Self-serve mode: whether users may create and edit datasets.
    pub dataset_editor: bool,
    /// Self-serve mode: the kinds of workspace users may open.
    pub workspace_kinds: Vec<WorkspaceKind>,
    /// Self-serve mode: whether users may create workspaces.
    pub create_workspaces: bool,
}

impl AnalyticsConfig {
    /// Read `config` — a framework's settings, defaults filling what is
    /// missing — or the sentence saying what is wrong with it. Everything that
    /// can be said without the database is checked here; that the workspaces
    /// it names exist is [`check_config_against`]'s.
    pub fn read(config: &Attrs) -> Result<AnalyticsConfig> {
        let spec = analytics_config_spec();
        let get = |name: &str| -> Option<Json> {
            spec.iter()
                .find(|f| f.name() == name)
                .and_then(|f| f.resolve(config).cloned())
                .filter(|v| !v.is_null())
        };
        let refuse =
            |what: String| Error::invalid(format!("framework `{ANALYTICS_FRAMEWORK}`: {what}"));
        let mode = match get(CFG_MODE).as_ref().and_then(Json::as_str) {
            Some("fixed") => Mode::Fixed,
            Some("self_serve") => Mode::SelfServe,
            other => {
                return Err(refuse(format!(
                    "`{CFG_MODE}` is `fixed` or `self_serve`, not {}",
                    other.map_or_else(|| "missing".to_owned(), |o| format!("`{o}`"))
                )));
            }
        };
        let min_role = get(CFG_MIN_ROLE)
            .as_ref()
            .and_then(Json::as_u64)
            .and_then(|r| u8::try_from(r).ok())
            .filter(|r| (1..=100).contains(r))
            .ok_or_else(|| refuse(format!("`{CFG_MIN_ROLE}` is a role from 1 to 100")))?;
        let strings = |name: &str, what: &str| -> Result<Vec<String>> {
            match get(name) {
                None => Ok(Vec::new()),
                Some(Json::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| {
                                refuse(format!("`{name}` is a list of {what}; {item} is not one"))
                            })
                    })
                    .collect(),
                Some(other) => Err(refuse(format!(
                    "`{name}` is a list of {what}, like [\"…\"]; got {other}"
                ))),
            }
        };
        let workspaces = strings(CFG_WORKSPACES, "workspace names or ids")?;
        let mut workspace_kinds = Vec::new();
        for name in strings(CFG_WORKSPACE_KINDS, "kinds of workspace")? {
            let kind = WorkspaceKind::parse(&name)
                .map_err(|e| refuse(format!("`{CFG_WORKSPACE_KINDS}`: {}", plain(&e))))?;
            kind.check_available()
                .map_err(|e| refuse(format!("`{CFG_WORKSPACE_KINDS}`: {}", plain(&e))))?;
            if !workspace_kinds.contains(&kind) {
                workspace_kinds.push(kind);
            }
        }
        let flag = |name: &str| get(name).as_ref().and_then(Json::as_bool).unwrap_or(false);
        let config = AnalyticsConfig {
            mode,
            min_role,
            workspaces,
            dataset_editor: flag(CFG_DATASET_EDITOR),
            workspace_kinds,
            create_workspaces: flag(CFG_CREATE_WORKSPACES),
        };
        match config.mode {
            Mode::Fixed if config.workspaces.is_empty() => Err(refuse(format!(
                "in fixed mode it shows the workspaces `{CFG_WORKSPACES}` names, and it names none"
            ))),
            Mode::SelfServe if config.workspace_kinds.is_empty() && !config.dataset_editor => {
                Err(refuse(format!(
                    "in self-serve mode users need something to do: name a kind of workspace in \
                     `{CFG_WORKSPACE_KINDS}`, or turn on `{CFG_DATASET_EDITOR}`"
                )))
            }
            _ => Ok(config),
        }
    }
}

/// The message of an error, without the kind's prefix.
fn plain(e: &Error) -> String {
    match e.repr() {
        sc_error::Repr::Invalid(m) => m.clone(),
        _ => e.to_string(),
    }
}

/// Check `config` against the database as it is now: every workspace it shows
/// exists, once, belongs to the unrestricted Analytics UI, and is of a kind
/// that can be opened. What `save_application` runs, so a name mistyped is
/// refused where the admin is still looking at the form.
pub async fn check_config_against(catalog: &Catalog, config: &Attrs) -> Result<()> {
    let read = AnalyticsConfig::read(config)?;
    if read.mode == Mode::Fixed {
        resolve_workspaces(catalog, &read.workspaces).await?;
    }
    Ok(())
}

/// The workspaces `refs` name, in order: each an id, or the name of exactly
/// one workspace of the unrestricted Analytics UI.
pub async fn resolve_workspaces(catalog: &Catalog, refs: &[String]) -> Result<Vec<Workspace>> {
    let refuse =
        |what: String| Error::invalid(format!("framework `{ANALYTICS_FRAMEWORK}`: {what}"));
    let mut all: Option<Vec<Workspace>> = None;
    let mut out: Vec<Workspace> = Vec::with_capacity(refs.len());
    for r in refs {
        let found = match r.parse::<Uuid>() {
            Ok(id) => load_workspace(catalog, WorkspaceId(id)).await?,
            Err(_) => {
                if all.is_none() {
                    all = Some(list_workspaces(catalog).await?);
                }
                let named: Vec<&Workspace> = all
                    .iter()
                    .flatten()
                    .filter(|w| w.application.is_none() && w.name == *r)
                    .collect();
                if named.len() > 1 {
                    return Err(refuse(format!(
                        "{} workspaces are called `{r}`; name this one by its id",
                        named.len()
                    )));
                }
                named.first().map(|w| (*w).clone())
            }
        };
        let Some(ws) = found else {
            return Err(refuse(format!("there is no workspace `{r}` to show")));
        };
        if ws.application.is_some() {
            return Err(refuse(format!(
                "the workspace `{}` belongs to an application; show one of the Analytics UI's own",
                ws.name
            )));
        }
        ws.kind.check_available().map_err(|e| refuse(plain(&e)))?;
        if !out.iter().any(|w| w.id == ws.id) {
            out.push(ws);
        }
    }
    Ok(out)
}

/// What one request through an Analytics application may reach (A9.4).
#[derive(Debug, Clone)]
pub struct AppScope {
    /// The application's id: what its workspaces belong to.
    pub app: Uuid,
    /// Its name, for the shell's header.
    pub name: String,
    /// Its settings.
    pub config: AnalyticsConfig,
    /// The application's tables: what self-serve datasets may read.
    pub tables: BTreeSet<String>,
    /// Fixed mode: the workspaces shown, in order.
    pub shown: Vec<Workspace>,
    /// Fixed mode: the datasets they read.
    pub datasets: BTreeSet<DatasetId>,
}

impl AppScope {
    /// The scope of the application `app`, whose framework settings are
    /// `config` and whose tables are `tables`, as the database is now.
    pub async fn load(
        catalog: &Catalog,
        app: Uuid,
        name: &str,
        config: &Attrs,
        tables: impl IntoIterator<Item = String>,
    ) -> Result<AppScope> {
        let config = AnalyticsConfig::read(config)?;
        let shown = match config.mode {
            Mode::Fixed => resolve_workspaces(catalog, &config.workspaces).await?,
            Mode::SelfServe => Vec::new(),
        };
        let datasets = shown.iter().flat_map(workspace_datasets).collect();
        Ok(AppScope {
            app,
            name: name.to_owned(),
            config,
            tables: tables.into_iter().collect(),
            shown,
            datasets,
        })
    }

    /// Whether a caller of `role` may use the application at all.
    pub fn admits_role(&self, role: u8) -> bool {
        role <= self.config.min_role
    }

    /// The caller a read in this application is guarded as: `caller`, and in
    /// self-serve mode only the application's tables.
    pub fn narrow(&self, caller: Caller) -> Caller {
        match self.config.mode {
            Mode::SelfServe => caller.within(self.tables.iter().cloned()),
            Mode::Fixed => caller,
        }
    }

    /// Whether `table` may be a dataset's base here.
    pub fn offers_table(&self, table: &str) -> bool {
        self.config.mode == Mode::SelfServe && self.tables.contains(table)
    }

    /// Whether `caller` sees the dataset `def`. Fixed: it is one the shown
    /// workspaces read. Self-serve: they own it or it is shared with them, and
    /// it is built on one of the application's tables.
    pub fn sees_dataset(
        &self,
        caller: &Caller,
        def: &DatasetDef,
        sharing: &Sharing,
        library: &Library,
    ) -> bool {
        match self.config.mode {
            Mode::Fixed => self.datasets.contains(&def.id),
            Mode::SelfServe => {
                sharing.admits(caller)
                    && root_table(library, def).is_some_and(|t| self.tables.contains(&t))
            }
        }
    }

    /// Refuse, unless datasets may be created and edited here.
    pub fn check_dataset_editor(&self) -> Result<()> {
        if self.config.mode == Mode::SelfServe && self.config.dataset_editor {
            Ok(())
        } else {
            Err(Error::auth(
                "datasets cannot be created or changed in this application",
            ))
        }
    }

    /// Whether `kind` may be opened here.
    pub fn allows_kind(&self, kind: WorkspaceKind) -> bool {
        match self.config.mode {
            Mode::Fixed => self.shown.iter().any(|w| w.kind == kind),
            Mode::SelfServe => self.config.workspace_kinds.contains(&kind),
        }
    }

    /// Whether `caller` sees the workspace `ws`. Fixed: it is shown. Self-serve:
    /// it belongs to this application, is of a kind it allows, and is theirs or
    /// shared with them.
    pub fn sees_workspace(&self, caller: &Caller, ws: &Workspace) -> bool {
        match self.config.mode {
            Mode::Fixed => self.shown.iter().any(|w| w.id == ws.id),
            Mode::SelfServe => {
                ws.application == Some(self.app)
                    && self.allows_kind(ws.kind)
                    && ws.sharing().admits(caller)
            }
        }
    }

    /// Refuse, unless `caller` may create a workspace of `kind` here.
    pub fn check_create_workspace(&self, kind: WorkspaceKind) -> Result<()> {
        if self.config.mode != Mode::SelfServe || !self.config.create_workspaces {
            return Err(Error::auth(
                "workspaces cannot be created in this application",
            ));
        }
        if !self.allows_kind(kind) {
            return Err(Error::auth(format!(
                "a {} workspace cannot be opened in this application",
                kind.label()
            )));
        }
        Ok(())
    }

    /// Refuse, unless `caller` may change `ws` — rename it, share it, save its
    /// state, delete it. Nothing shown in fixed mode is changed by its viewers.
    pub fn check_change_workspace(&self, caller: &Caller, ws: &Workspace) -> Result<()> {
        if self.config.mode == Mode::SelfServe
            && self.sees_workspace(caller, ws)
            && ws.sharing().may_change(caller)
        {
            Ok(())
        } else {
            Err(Error::auth(format!(
                "you may not change the workspace `{}`",
                ws.name
            )))
        }
    }

    /// What the restricted shell is told about the application: its name and
    /// settings, and in fixed mode the workspaces it shows.
    pub fn shell_json(&self) -> Json {
        json!({
            "id": self.app,
            "name": self.name,
            "mode": self.config.mode,
            "dataset_editor": self.config.mode == Mode::SelfServe && self.config.dataset_editor,
            "workspace_kinds": match self.config.mode {
                Mode::Fixed => self
                    .shown
                    .iter()
                    .map(|w| w.kind.as_str())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>(),
                Mode::SelfServe => self
                    .config
                    .workspace_kinds
                    .iter()
                    .map(|k| k.as_str())
                    .collect(),
            },
            "create_workspaces":
                self.config.mode == Mode::SelfServe && self.config.create_workspaces,
            "workspaces": self
                .shown
                .iter()
                .map(|w| json!({ "id": w.id, "name": w.name, "kind": w.kind.as_str() }))
                .collect::<Vec<_>>(),
        })
    }
}

/// The table `def`'s chain of bases starts from, if it reaches one.
pub fn root_table(library: &Library, def: &DatasetDef) -> Option<String> {
    let mut seen: BTreeMap<DatasetId, ()> = BTreeMap::new();
    let mut at = def;
    loop {
        match &at.base {
            Base::Table { table } => return Some(table.clone()),
            Base::Dataset { dataset } => {
                if seen.insert(*dataset, ()).is_some() {
                    return None;
                }
                at = library.get(*dataset)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(value: Json) -> Attrs {
        serde_json::from_value(value).expect("an object")
    }

    #[test]
    fn the_settings_read_with_their_defaults_and_each_mode_is_checked() {
        let fixed = AnalyticsConfig::read(&attrs(json!({
            "mode": "fixed", "min_role": 40, "workspaces": ["Incidents"]
        })))
        .expect("reads");
        assert_eq!(fixed.mode, Mode::Fixed);
        assert_eq!(fixed.min_role, 40);
        assert_eq!(fixed.workspaces, vec!["Incidents".to_owned()]);
        assert!(!fixed.dataset_editor);

        let serve = AnalyticsConfig::read(&attrs(json!({
            "mode": "self_serve", "min_role": 40, "dataset_editor": true,
            "workspace_kinds": ["data_explorer", "data_explorer"]
        })))
        .expect("reads");
        assert_eq!(serve.workspace_kinds, vec![WorkspaceKind::DataExplorer]);
        // The default lets users create the kinds they may open.
        assert!(serve.create_workspaces);

        // The role floor defaults to the admin's.
        let default = AnalyticsConfig::read(&attrs(json!({ "workspaces": ["A"] }))).expect("reads");
        assert_eq!((default.mode, default.min_role), (Mode::Fixed, 1));

        let refused = |value: Json, says: &str| {
            let err = AnalyticsConfig::read(&attrs(value)).expect_err("refused");
            assert!(err.to_string().contains(says), "{err}");
        };
        refused(json!({ "mode": "fixed" }), "names none");
        refused(
            json!({ "mode": "sometimes", "workspaces": ["A"] }),
            "`fixed` or `self_serve`",
        );
        refused(
            json!({ "min_role": 0, "workspaces": ["A"] }),
            "from 1 to 100",
        );
        refused(
            json!({ "min_role": 101, "workspaces": ["A"] }),
            "from 1 to 100",
        );
        refused(json!({ "workspaces": "A" }), "is a list of workspace names");
        refused(json!({ "workspaces": [""] }), "is not one");
        refused(
            json!({ "mode": "self_serve", "workspace_kinds": ["spreadsheet"] }),
            "not a kind of workspace",
        );
        refused(
            json!({ "mode": "self_serve", "workspace_kinds": ["simulation"] }),
            "milestone A7",
        );
        refused(
            json!({ "mode": "self_serve", "workspace_kinds": [] }),
            "need something to do",
        );
    }

    #[test]
    fn the_spec_shows_each_modes_settings_only_in_that_mode() {
        let spec = analytics_config_spec();
        let applies = |name: &str, mode: &str| {
            spec.iter()
                .find(|f| f.name() == name)
                .expect("declared")
                .applies(&spec, &attrs(json!({ "mode": mode })))
        };
        assert!(applies(CFG_WORKSPACES, "fixed"));
        assert!(!applies(CFG_WORKSPACES, "self_serve"));
        for name in [
            CFG_DATASET_EDITOR,
            CFG_WORKSPACE_KINDS,
            CFG_CREATE_WORKSPACES,
        ] {
            assert!(
                applies(name, "self_serve") && !applies(name, "fixed"),
                "{name}"
            );
        }
        assert!(applies(CFG_MIN_ROLE, "fixed") && applies(CFG_MIN_ROLE, "self_serve"));
        // The structural check every framework's settings go through accepts
        // what `read` accepts.
        sc_types::validate_attrs(
            &spec,
            &attrs(json!({ "mode": "self_serve", "min_role": 40, "dataset_editor": true })),
        )
        .expect("valid");
    }

    fn scope(mode: Json) -> AppScope {
        let config = AnalyticsConfig::read(&attrs(mode)).expect("reads");
        AppScope {
            app: Uuid::new_v4(),
            name: "Insights".into(),
            config,
            tables: BTreeSet::from(["houses".to_owned(), "neighbourhoods".to_owned()]),
            shown: Vec::new(),
            datasets: BTreeSet::new(),
        }
    }

    #[test]
    fn self_serve_sees_its_own_and_shared_datasets_on_its_tables() {
        let app = scope(json!({
            "mode": "self_serve", "min_role": 40, "dataset_editor": true,
            "workspace_kinds": ["data_explorer"]
        }));
        let user = Uuid::new_v4();
        let caller = Caller {
            role: 40,
            user: Some(BTreeMap::from([(
                "id".to_owned(),
                sc_query::Value::Uuid(user),
            )])),
            tables: None,
        };
        let houses = DatasetDef::new("Houses", Base::table("houses"));
        let on_houses = DatasetDef::new("Mine", Base::dataset(houses.id));
        let incidents = DatasetDef::new("Incidents", Base::table("incidents"));
        let library = Library::new([houses.clone(), on_houses.clone(), incidents.clone()]);
        let mine = Sharing::owned_by(Some(user));
        let admins = Sharing::default();
        let shared = Sharing {
            share_role: Some(80),
            ..admins
        };
        assert!(app.sees_dataset(&caller, &houses, &mine, &library));
        // Based on a dataset based on `houses`.
        assert!(app.sees_dataset(&caller, &on_houses, &mine, &library));
        assert!(!app.sees_dataset(&caller, &houses, &admins, &library));
        assert!(app.sees_dataset(&caller, &houses, &shared, &library));
        // Not one of the application's tables, whoever owns it.
        assert!(!app.sees_dataset(&caller, &incidents, &mine, &library));
        assert!(app.offers_table("houses") && !app.offers_table("incidents"));
        app.check_dataset_editor().expect("on");
        // Reads are guarded to the application's tables.
        let narrowed = app.narrow(caller.clone());
        assert!(narrowed.may_name("houses") && !narrowed.may_name("incidents"));
        assert!(app.admits_role(40) && !app.admits_role(80));

        let mut ws = Workspace::new("Mine", WorkspaceKind::DataExplorer, Some(user));
        assert!(!app.sees_workspace(&caller, &ws), "not this application's");
        ws = ws.in_application(app.app);
        assert!(app.sees_workspace(&caller, &ws));
        app.check_change_workspace(&caller, &ws).expect("theirs");
        app.check_create_workspace(WorkspaceKind::DataExplorer)
            .expect("allowed");
        assert!(app.check_create_workspace(WorkspaceKind::Map).is_err());
        let mut report = Workspace::new("R", WorkspaceKind::Report, Some(user));
        report = report.in_application(app.app);
        assert!(!app.sees_workspace(&caller, &report), "not a kind it opens");
    }

    #[test]
    fn fixed_mode_sees_what_its_workspaces_read_and_changes_nothing() {
        let mut app = scope(json!({ "mode": "fixed", "min_role": 40, "workspaces": ["D"] }));
        let d = DatasetDef::new("Incidents", Base::table("incidents"));
        let other = DatasetDef::new("Houses", Base::table("houses"));
        let library = Library::new([d.clone(), other.clone()]);
        let ws = Workspace::new("D", WorkspaceKind::Dashboard, None);
        app.shown = vec![ws.clone()];
        app.datasets = BTreeSet::from([d.id]);
        let caller = Caller {
            role: 40,
            user: None,
            tables: None,
        };
        // Not one of the application's tables: fixed mode does not ask.
        assert!(app.sees_dataset(&caller, &d, &Sharing::default(), &library));
        assert!(!app.sees_dataset(&caller, &other, &Sharing::default(), &library));
        assert!(app.sees_workspace(&caller, &ws));
        assert!(app.allows_kind(WorkspaceKind::Dashboard));
        assert!(!app.allows_kind(WorkspaceKind::DataExplorer));
        assert!(app.check_change_workspace(&caller, &ws).is_err());
        assert!(
            app.check_create_workspace(WorkspaceKind::Dashboard)
                .is_err()
        );
        assert!(app.check_dataset_editor().is_err());
        assert!(!app.offers_table("houses"));
        // No table subset: the shown workspaces' datasets are the grant.
        assert!(app.narrow(caller).tables.is_none());
        assert_eq!(app.shell_json()["workspace_kinds"], json!(["dashboard"]));
    }
}
