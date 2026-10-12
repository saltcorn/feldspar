//! Saltcorn UI: v1-style views and pages, owned by an application (layer 8;
//! TODO "Saltcorn UI" §1, §4).
//!
//! A **view** is a view pattern (v1 calls it a view template) configured over one
//! table; a **page** is a layout that places views. In Saltcorn 1 both are global
//! to a tenant. Here the unit of multi-tenancy is the [`Application`](sc_app::Application),
//! so both belong to one: `_fd_views.application` and `_fd_pages.application` are
//! columns, a name is unique **per application**, and a view may only name a table
//! in its application's subset. Everything else about a view is v1's — the
//! pattern name, the `min_role`, the slug, and a `configuration` that is
//! **v1-shaped and deliberately untouched**, because it is what v1's own
//! `list.ts` reads.
//!
//! What is here:
//!
//! - [`View`] / [`Page`], their ids, and the two tables ([`bootstrap`]).
//! - The row path, scoped by [`AppId`](sc_app::AppId): [`save_view`],
//!   [`load_view`], [`list_views`], [`delete_view`] and the page four, with the
//!   save-time validation of §1 and §11 — the pattern is registered, the table is
//!   in the application's subset, `min_role` is a role that exists, and the name
//!   is unique in the application and usable as a URL path segment.
//! - The pattern registry ([`registered_patterns`]): v1's six built-in patterns,
//!   and whatever an installed module declares (Phase 11) — and the headers and
//!   `public/` directory such a module brings with its patterns
//!   ([`installed_plugin_assets`]).
//! - [`LibraryItem`] — a named, reusable layout fragment of one Saltcorn UI
//!   application (TODO "The builder" §8) — its row path ([`save_library_item`],
//!   [`list_library`], …), [`apply_library_updates`] in one transaction, and the
//!   layout walk its references are built from ([`placed_library_ids`]).
//! - [`ViewSet`] — every view, page and library item of one application, loaded
//!   once — and
//!   [`ViewSets`], the cache that reloads a set on a write and stamps it with the
//!   **generation** the worker's view snapshot is keyed on (§4).
//! - Where the view runtime is ([`require_view_runtime`]): the `ui/saltcorn-ui`
//!   bundle, and the sentence an application that needs it fails to mount with
//!   when this server was built without it.
//! - [`ViewSnapshot`] — an application's views, pages and settings serialised at
//!   one generation, which is what v1's synchronous `View.findOne` is answered
//!   from on the worker (§4).
//! - [`ViewRuntime`] — the seam a view is rendered, posted to and configured
//!   through, declared here and implemented by `sc-module` over its worker
//!   (§3), and the one installed at boot ([`view_runtime`]).
//! - [`SaltcornUiFramework`] — the framework that serves an application's views
//!   and pages on its subdomain, constructed by [`SaltcornUiFactory`] from
//!   `sc-app`'s factory registry ([`install_saltcorn_ui`]).

mod bundle;
mod configure;
mod framework;
mod layout;
mod library;
mod patterns;
mod plugins;
mod runtime;
mod snapshot;
mod store;
mod tables;
mod validate;
mod view;
mod view_set;

pub use bundle::{
    BUNDLE_DIR_IN_CHECKOUT, SALTCORN_UI_FRAMEWORK, VIEW_RUNTIME_FILE, require_view_runtime,
};
pub use configure::{Configurer, check_step_values};
pub use framework::{
    ASSET_VERSION_TAG, CFG_ROOT_PAGES, CFG_SITE_NAME, DERIVED_CONFIG_KEYS, SaltcornUiFactory,
    SaltcornUiFramework, check_saltcorn_ui_config, install_saltcorn_ui, saltcorn_ui_config_spec,
    saltcorn_ui_csp, serve_store_file, view_sets,
};
pub use layout::{
    PAGE_ACTIONS, PageReferences, check_library_placements, check_page_actions, check_page_views,
    check_update_placements, merge_view_layout, referenced_pages, save_page_with_library_updates,
    save_view_with_library_updates, validate_page,
};
pub use library::{
    LibraryItem, LibraryItemId, LibraryReferences, LibraryUpdate, apply_library_updates,
    apply_library_updates_in, delete_library_item, list_library, load_library_item,
    placed_library_ids, save_library_item,
};
pub use patterns::{
    BUILTIN_PATTERNS, PatternInfo, builtin_patterns, find_pattern, install_patterns,
    registered_patterns,
};
pub use plugins::{
    PluginAssets, PluginHeader, install_plugin_assets, installed_plugin_assets, plugin_header_tags,
    plugin_public_file,
};
pub use runtime::{
    ConfigStep, Flash, PatternManifest, ViewContext, ViewOutput, ViewReferences, ViewRequest,
    ViewRuntime, ViewUser, Wrap, install_view_runtime, view_runtime,
};
pub use snapshot::{MENU_CONFIG_KEY, ViewSnapshot};
pub use store::{
    delete_application_views_pages_and_library, delete_page, delete_view, list_pages, list_views,
    load_page, load_view, save_page, save_view, validate_view,
};
pub use tables::{
    COL_APPLICATION, COL_ATTRIBUTES, COL_CONFIGURATION, COL_DESCRIPTION, COL_ICON, COL_ID,
    COL_LAYOUT, COL_MIN_ROLE, COL_NAME, COL_SLUG, COL_TABLE_NAME, COL_TITLE, COL_VIEWPATTERN,
    LIBRARY_TABLE, PAGES_TABLE, VIEWS_TABLE, bootstrap,
};
pub use validate::{
    VIEW_ACTIONS, check_name, check_view_actions, configured_actions, referenced_views,
};
pub use view::{Page, PageId, View, ViewId};
pub use view_set::{ViewSet, ViewSets};
