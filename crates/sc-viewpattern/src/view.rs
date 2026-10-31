//! [`View`] and [`Page`]: the two records a Saltcorn UI application is made of
//! (TODO "Saltcorn UI" §1).

use sc_app::AppId;
use sc_types::Attrs;
use sc_types::Role;
use serde_json::Value as Json;
use uuid::Uuid;

/// Identifies a view: the UUID primary key of its `_fd_views` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewId(pub Uuid);

impl ViewId {
    /// Mint an id for a new view.
    pub fn new() -> ViewId {
        ViewId(Uuid::new_v4())
    }
}

impl Default for ViewId {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifies a page: the UUID primary key of its `_fd_pages` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub Uuid);

impl PageId {
    /// Mint an id for a new page.
    pub fn new() -> PageId {
        PageId(Uuid::new_v4())
    }
}

impl Default for PageId {
    fn default() -> Self {
        Self::new()
    }
}

/// One view: a view pattern configured over a table, in one application.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// The row's identity.
    pub id: ViewId,
    /// The application the view belongs to (§1 — the departure from v1).
    pub application: AppId,
    /// The name, unique within the application; it is the `/view/:name` path
    /// segment.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// The view pattern's name, as v1 spells it (`List`, `Show`, …).
    pub viewpattern: String,
    /// The table the view is over, which must be in the application's subset.
    /// `None` only for a pattern that declares itself tableless.
    pub table_name: Option<String>,
    /// The pattern's configuration, **exactly as v1 shaped it**. Nothing here
    /// reads or translates it; the pattern's own source does.
    pub configuration: Attrs,
    /// The least privileged role that may see the view (`1..=100`, lower is more
    /// privileged).
    pub min_role: Role,
    /// v1's slug — `{label, steps}` or absent — carried as it came.
    pub slug: Option<Json>,
    /// Sparse per-view settings (§9).
    pub attributes: Attrs,
}

impl View {
    /// A view of `viewpattern` over `table` in `application`, with an empty
    /// configuration, visible to everyone.
    pub fn new(
        application: AppId,
        name: impl Into<String>,
        viewpattern: impl Into<String>,
        table: impl Into<String>,
    ) -> View {
        View {
            id: ViewId::new(),
            application,
            name: name.into(),
            description: String::new(),
            viewpattern: viewpattern.into(),
            table_name: Some(table.into()),
            configuration: Attrs::new(),
            min_role: Role::PUBLIC,
            slug: None,
            attributes: Attrs::new(),
        }
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> View {
        self.description = description.into();
        self
    }

    /// Set the configuration.
    pub fn configuration(mut self, configuration: Attrs) -> View {
        self.configuration = configuration;
        self
    }

    /// Set the minimum role.
    pub fn min_role(mut self, role: Role) -> View {
        self.min_role = role;
        self
    }
}

/// One page: a layout placing views, in one application.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The row's identity.
    pub id: PageId,
    /// The application the page belongs to.
    pub application: AppId,
    /// The name, unique within the application; it is the `/page/:name` path
    /// segment.
    pub name: String,
    /// The document title.
    pub title: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// The layout, exactly as v1 shaped it.
    pub layout: Json,
    /// The least privileged role that may see the page.
    pub min_role: Role,
    /// Sparse per-page settings (§9) — including `root_page_for_roles`, which is
    /// what makes `/` resolve.
    pub attributes: Attrs,
}

impl Page {
    /// A page named `name` in `application` with an empty layout, visible to
    /// everyone.
    pub fn new(application: AppId, name: impl Into<String>) -> Page {
        Page {
            id: PageId::new(),
            application,
            name: name.into(),
            title: String::new(),
            description: String::new(),
            layout: Json::Object(Attrs::new()),
            min_role: Role::PUBLIC,
            attributes: Attrs::new(),
        }
    }

    /// Set the title.
    pub fn title(mut self, title: impl Into<String>) -> Page {
        self.title = title.into();
        self
    }

    /// Set the layout.
    pub fn layout(mut self, layout: Json) -> Page {
        self.layout = layout;
        self
    }

    /// Set the minimum role.
    pub fn min_role(mut self, role: Role) -> Page {
        self.min_role = role;
        self
    }
}
