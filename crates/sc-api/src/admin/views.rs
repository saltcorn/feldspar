//! The admin API's endpoints: saltcorn UI views and pages, and the builder.
//!
//! `sc-server`'s `handlers::views` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    // A Saltcorn UI application's source is rows rather than a file store
    // (TODO "Saltcorn UI" §1): its views and pages, each addressed by name
    // within the application. A save is the whole deployment — the mounted app
    // reads the reloaded set on its next request — so there is no build to
    // follow one with.
    //
    // `saveView`/`savePage` save the record the path names, creating it when
    // there is none; a body naming something else renames it. What a view may
    // name — the pattern, the table, the role, the actions — is refused on save
    // with a sentence naming it, and the configuration is replayed through the
    // pattern's own configuration steps, so a value a step's form would not
    // accept is refused naming the step and the field (Phase 10).
    set.register(
        Endpoint::new(
            "listViews",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views"),
        )
        .output(TypeSchema::array(view_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getView",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text),
        )
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "saveView",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text),
        )
        .input(view_input_schema())
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteView",
            Method::Delete,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // A new view (TODO "Saltcorn UI" 10.2): its name, pattern, table and role,
    // configured as the pattern's `initial_config` starts one — a List over its
    // table's columns. Refused as a save is, and refused if the name is taken,
    // where a save would overwrite.
    set.register(
        Endpoint::new(
            "createView",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views"),
        )
        .input(create_view_input_schema())
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    // One step of a view's configuration wizard (10.1): the pattern's
    // `configuration_workflow` step `step`, over the table and the context the
    // earlier steps gathered, as the form fields the admin UI renders. A call
    // per step rather than a form per pattern, because a step's form does not
    // exist without its context: List's *Default state* lists the table's
    // fields, and a ListShowList's views are the views over its table.
    set.register(
        Endpoint::new(
            "viewConfigStep",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("view-config-step"),
        )
        .input(view_config_step_input_schema())
        .output(view_config_step_schema())
        .auth(AuthRequirement::admin()),
    );

    // What refers to a view by name (10.4) — the views that embed it or link
    // to it, by their patterns' own `connectedObjects`, and the pages that show
    // it — so a rename can say before it happens what it will leave pointing at
    // a name that no longer exists. Nothing is rewritten.
    set.register(
        Endpoint::new(
            "viewReferences",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text)
                .lit("references"),
        )
        .output(view_references_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "listPages",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages"),
        )
        .output(TypeSchema::array(page_schema()))
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "getPage",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text),
        )
        .output(page_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "savePage",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text),
        )
        .input(page_input_schema())
        .output(page_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deletePage",
            Method::Delete,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // What `ui/builder` calls, and nothing it does not: the two layout saves,
    // what names a page, the application's library, and the calls the canvas
    // makes for its previews and lookups — v1's server routes, ported into the
    // worker and made as the admin. Every one refuses an application that is not
    // a Saltcorn UI application. The builder's options are not an endpoint: the
    // builder route renders them into its document.

    // A view's layout from one of its pattern's builder steps (§6): merged into
    // the configuration where the step keeps it, checked as `saveView` checks —
    // the store, the replay of the other steps, the actions — and saved with the
    // library edits it carries in one transaction, moving the generation once.
    set.register(
        Endpoint::new(
            "saveViewLayout",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("views")
                .param("name", ValueType::Text)
                .lit("layout"),
        )
        .input(view_layout_input_schema())
        .output(view_schema())
        .auth(AuthRequirement::admin()),
    );

    // A page's layout (§6): it replaces the page's, is checked as a page is —
    // its actions are v1's page actions or the application's triggers, and the
    // views it shows are the application's — and is saved with its library
    // edits in one transaction.
    set.register(
        Endpoint::new(
            "savePageLayout",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text)
                .lit("layout"),
        )
        .input(page_layout_input_schema())
        .output(page_schema())
        .auth(AuthRequirement::admin()),
    );

    // What names a page (§10): the menu entries opening it, the roles whose home
    // page it is, and the views, pages and library items whose layouts show or
    // link to it — so a rename or a delete can say beforehand what it leaves
    // pointing at a name that no longer exists. `places` is the other direction:
    // the library items its own layout places. Nothing is rewritten.
    set.register(
        Endpoint::new(
            "pageReferences",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("pages")
                .param("name", ValueType::Text)
                .lit("references"),
        )
        .output(page_references_schema())
        .auth(AuthRequirement::admin()),
    );

    // The application's library (§8): its items, each with the views, pages
    // and other items that place it.
    set.register(
        Endpoint::new(
            "listLibrary",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library"),
        )
        .output(TypeSchema::array(listed_library_item_schema()))
        .auth(AuthRequirement::admin()),
    );

    // One item, read fresh rather than from the set the builder was opened
    // with: v1's `/library/content/:id`, which a placed instance starts from so
    // it shows the latest layout.
    set.register(
        Endpoint::new(
            "getLibraryItem",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .param("item", ValueType::Uuid),
        )
        .output(library_item_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/library/savefrombuilder`: a new item from what the builder
    // selected. A name the application's library already has is refused naming
    // it, as is a layout placing an item the application does not have.
    set.register(
        Endpoint::new(
            "createLibraryItem",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library"),
        )
        .input(create_library_item_input_schema())
        .output(library_item_schema())
        .auth(AuthRequirement::admin()),
    );

    // Rename an item, or change its icon or description. Its layout is the
    // builder's, and changes through a layout save or `saveLibraryUpdates`.
    set.register(
        Endpoint::new(
            "saveLibraryItem",
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .param("item", ValueType::Uuid),
        )
        .input(save_library_item_input_schema())
        .output(library_item_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/library/save-updates`: edits made inside placed items, apart from
    // any view or page, all or none.
    set.register(
        Endpoint::new(
            "saveLibraryUpdates",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .lit("updates"),
        )
        .input(library_updates_input_schema())
        .output(TypeSchema::struct_of([StructField::new(
            "updated",
            TypeSchema::int(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Delete an item. One that something places is refused with `409` and the
    // `references` beside the error, unless `confirm` is true; what placed it
    // then renders blank, which is v1's `resolveSegment` behaviour. The answer
    // names what did.
    set.register(
        Endpoint::new(
            "deleteLibraryItem",
            Method::Delete,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("library")
                .param("item", ValueType::Uuid),
        )
        .query([QueryParam::new("confirm", ValueType::Bool)])
        .output(TypeSchema::struct_of([
            StructField::new("deleted", TypeSchema::bool()),
            StructField::new("references", library_references_schema()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // v1's `/field/preview/:table/:field/:fieldview`: a fieldview rendered over
    // the first row the admin can read, as HTML for the canvas.
    set.register(
        Endpoint::new(
            "builderFieldPreview",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("field-preview"),
        )
        .input(builder_field_preview_input_schema())
        .output(builder_html_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/field/fieldviewcfgform/:table?accept=json`: a fieldview's
    // configuration fields, as v1's form JSON — which is the builder's to render,
    // so it is `json` here.
    set.register(
        Endpoint::new(
            "builderFieldviewConfigForm",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("fieldview-config"),
        )
        .input(builder_fieldview_config_input_schema())
        .output(TypeSchema::json())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/view/:name/preview`: an embedded view rendered for the canvas with
    // the state given.
    set.register(
        Endpoint::new(
            "builderViewPreview",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("view-preview"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("view", TypeSchema::text()),
            StructField::new("state", TypeSchema::optional(TypeSchema::json())),
        ]))
        .output(builder_html_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/page/:name/preview`: an embedded page rendered for the canvas.
    set.register(
        Endpoint::new(
            "builderPagePreview",
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("page-preview"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "page",
            TypeSchema::text(),
        )]))
        .output(builder_html_schema())
        .auth(AuthRequirement::admin()),
    );

    // v1's `/api/:table/distinct/:field`, which the builder's *Tabs* element
    // asks for a field's values: v1's `{ success: [...] }`, as the admin, for a
    // table in the application's subset only (§3). v1's public row API behind it
    // is not a route on the subdomain, and this does not add one.
    set.register(
        Endpoint::new(
            "builderDistinctValues",
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("builder")
                .lit("distinct")
                .param("table", ValueType::Text)
                .param("field", ValueType::Text),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "success",
            TypeSchema::json(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // The view patterns a view may be saved with: the registry save checks
    // against, described by the view runtime where one is running. Server-wide
    // rather than per application, because a pattern is the server's.
    set.register(
        Endpoint::new("listViewPatterns", Method::Get, api().lit("view-patterns"))
            .output(TypeSchema::array(view_pattern_schema()))
            .auth(AuthRequirement::admin()),
    );

    // Whether this server has the builder bundle (TODO "The builder" §2, §9), so
    // the admin UI offers **Open in builder** only where it opens something. A
    // binary built without `ui/builder` answers `false`, and the admin screens
    // keep a layout as read-only JSON with a sentence saying why.
    set.register(
        Endpoint::new("builderStatus", Method::Get, api().lit("builder"))
            .output(TypeSchema::struct_of([StructField::new(
                "available",
                TypeSchema::bool(),
            )]))
            .auth(AuthRequirement::admin()),
    );
}

/// A Saltcorn UI view's fields on the wire, less its id. `configuration` is
/// v1's, unchanged; `slug` is v1's `{label, steps}` or absent.
fn view_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("viewpattern", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("configuration", TypeSchema::json()),
        StructField::new("min_role", TypeSchema::int()),
        StructField::new("slug", TypeSchema::optional(TypeSchema::json())),
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// A Saltcorn UI view as returned: its id and [`view_fields`].
fn view_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(view_fields());
    TypeSchema::Struct(fields)
}

/// The body `saveView` takes: the view less its id, which is the stored view's
/// when the path names one and fresh otherwise.
fn view_input_schema() -> TypeSchema {
    TypeSchema::Struct(view_fields())
}

/// The body `createView` takes: what the admin chooses. The configuration is
/// the pattern's to supply.
fn create_view_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("viewpattern", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("min_role", TypeSchema::int()),
    ])
}

/// The body `viewConfigStep` takes: the pattern and table being configured, the
/// view's name (absent for one not yet saved), the step, counting from 0, and
/// the configuration gathered so far.
fn view_config_step_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("viewpattern", TypeSchema::text()),
        StructField::new("table_name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("name", TypeSchema::optional(TypeSchema::text())),
        StructField::new("step", TypeSchema::int()),
        StructField::new("context", TypeSchema::json()),
    ])
}

/// One step of a view's configuration. `builder` is a layout step, and
/// `builder_options` the options v1's builder is opened with for it (none for a
/// form step); `skip` is a step v1 leaves out for this configuration;
/// `context_field` is the configuration key its values are kept under (none: the
/// top level); `values` are what its form opens with; `issues` are what the form
/// could not express faithfully.
fn view_config_step_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("index", TypeSchema::int()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("count", TypeSchema::int()),
        StructField::new("builder", TypeSchema::bool()),
        StructField::new("builder_options", TypeSchema::optional(TypeSchema::json())),
        StructField::new("skip", TypeSchema::bool()),
        StructField::new("context_field", TypeSchema::optional(TypeSchema::text())),
        StructField::new("blurb", TypeSchema::optional(TypeSchema::text())),
        StructField::new("fields", TypeSchema::array(form_field_schema())),
        StructField::new("values", TypeSchema::json()),
        StructField::new("issues", TypeSchema::array(TypeSchema::text())),
    ])
}

/// What refers to a view by name: the views embedding it, the views linking to
/// it, the pages and library items showing it — and the library items it places.
fn view_references_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("embedded_in", TypeSchema::array(TypeSchema::text())),
        StructField::new("linked_from", TypeSchema::array(TypeSchema::text())),
        StructField::new("pages", TypeSchema::array(TypeSchema::text())),
        // The library items whose layouts show or link to it (TODO "The
        // builder" §8).
        StructField::new("library", TypeSchema::array(TypeSchema::text())),
        // The other direction: the library items its own layout places.
        StructField::new("places", TypeSchema::array(TypeSchema::text())),
    ])
}

/// A Saltcorn UI page's fields on the wire, less its id. `attributes` carries
/// `root_page_for_roles`.
fn page_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("title", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("layout", TypeSchema::json()),
        StructField::new("min_role", TypeSchema::int()),
        StructField::new("attributes", TypeSchema::json()),
    ]
}

/// A Saltcorn UI page as returned: its id and [`page_fields`].
fn page_schema() -> TypeSchema {
    let mut fields = vec![StructField::new("id", TypeSchema::uuid())];
    fields.extend(page_fields());
    TypeSchema::Struct(fields)
}

/// The body `savePage` takes.
fn page_input_schema() -> TypeSchema {
    TypeSchema::Struct(page_fields())
}

/// One of v1's in-place edits to a placed library item: the item, and its whole
/// new layout.
fn library_update_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("library_id", TypeSchema::uuid()),
        StructField::new("layout", TypeSchema::json()),
    ])
}

/// The body `saveViewLayout` takes: the builder step, counting from 0, what the
/// builder wrote for it (v1's `columns` and `layout`), and its edits to the
/// library items the layout places.
fn view_layout_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("step", TypeSchema::int()),
        StructField::new("columns", TypeSchema::json()),
        StructField::new("layout", TypeSchema::json()),
        StructField::new(
            "libraryUpdates",
            TypeSchema::optional(TypeSchema::array(library_update_schema())),
        ),
    ])
}

/// The body `savePageLayout` takes: the page's new layout and the library edits
/// it carries.
fn page_layout_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("layout", TypeSchema::json()),
        StructField::new(
            "libraryUpdates",
            TypeSchema::optional(TypeSchema::array(library_update_schema())),
        ),
    ])
}

/// What names a page: the menu entries' labels, the roles (by name) whose home
/// page it is, and the views, pages and library items showing or linking to it —
/// and the library items its own layout places.
fn page_references_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("menu", TypeSchema::array(TypeSchema::text())),
        StructField::new("home_page_for", TypeSchema::array(TypeSchema::text())),
        StructField::new("views", TypeSchema::array(TypeSchema::text())),
        StructField::new("pages", TypeSchema::array(TypeSchema::text())),
        StructField::new("library", TypeSchema::array(TypeSchema::text())),
        StructField::new("places", TypeSchema::array(TypeSchema::text())),
    ])
}

/// What places a library item, by name: views, pages and other items, each
/// directly or through an item they place.
fn library_references_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("views", TypeSchema::array(TypeSchema::text())),
        StructField::new("pages", TypeSchema::array(TypeSchema::text())),
        StructField::new("library", TypeSchema::array(TypeSchema::text())),
    ])
}

/// A library item as returned: v1's `{ name, icon, layout }`, with this server's
/// UUID id, description and attributes. The layout is v1's, untouched.
fn library_item_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("icon", TypeSchema::text()),
        StructField::new("layout", TypeSchema::json()),
        StructField::new("attributes", TypeSchema::json()),
    ])
}

/// A library item as `listLibrary` returns it: [`library_item_schema`] and what
/// places it.
fn listed_library_item_schema() -> TypeSchema {
    let TypeSchema::Struct(mut fields) = library_item_schema() else {
        unreachable!("library_item_schema is a struct")
    };
    fields.push(StructField::new("used_by", library_references_schema()));
    TypeSchema::Struct(fields)
}

/// The body `createLibraryItem` takes: v1's `savefrombuilder` body, and a
/// description.
fn create_library_item_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("icon", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
        StructField::new("layout", TypeSchema::json()),
    ])
}

/// The body `saveLibraryItem` takes: the name, and the icon and description when
/// they change.
fn save_library_item_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("icon", TypeSchema::optional(TypeSchema::text())),
        StructField::new("description", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// The body `saveLibraryUpdates` takes: v1's `save-updates` body.
fn library_updates_input_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new(
        "libraryUpdates",
        TypeSchema::array(library_update_schema()),
    )])
}

/// A preview, as the HTML v1's route sends for the canvas.
fn builder_html_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new("html", TypeSchema::text())])
}

/// The body `builderFieldPreview` takes: v1's path (table, field, fieldview)
/// and v1's body (the fieldview's configuration, and the row a Show previews).
fn builder_field_preview_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("table", TypeSchema::text()),
        StructField::new("field", TypeSchema::text()),
        StructField::new("fieldview", TypeSchema::text()),
        StructField::new("configuration", TypeSchema::optional(TypeSchema::json())),
        StructField::new("row_id", TypeSchema::optional(TypeSchema::json())),
    ])
}

/// The body `builderFieldviewConfigForm` takes: v1's path (the table) and v1's
/// body, whose members say which kind of column asks — a field, a join field or
/// an aggregation.
fn builder_fieldview_config_input_schema() -> TypeSchema {
    let optional = |name: &str| StructField::new(name, TypeSchema::optional(TypeSchema::text()));
    TypeSchema::struct_of([
        StructField::new("table", TypeSchema::text()),
        optional("field_name"),
        optional("fieldview"),
        optional("type"),
        optional("join_field"),
        optional("join_fieldview"),
        optional("agg_outcome_type"),
        optional("agg_fieldview"),
        optional("agg_field"),
        optional("mode"),
        optional("_columndef"),
    ])
}

/// A registered view pattern: the name a view's `viewpattern` holds, how the
/// picker presents it, and what the runtime's manifest says about it. `label`,
/// `description`, `view_quantity`, `routes` and `steps` come from the running
/// view runtime; with none running they are the name and empty.
fn view_pattern_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("table_required", TypeSchema::bool()),
        StructField::new("view_quantity", TypeSchema::optional(TypeSchema::text())),
        StructField::new("routes", TypeSchema::array(TypeSchema::text())),
        StructField::new("steps", TypeSchema::array(TypeSchema::text())),
        // The module that declared it; absent for a built-in.
        StructField::new("module", TypeSchema::optional(TypeSchema::text())),
    ])
}
