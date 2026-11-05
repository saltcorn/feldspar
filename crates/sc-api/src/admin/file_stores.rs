//! The admin API's endpoints: file stores (their configuration) and the backend operations a
//! store's settings form runs.
//!
//! `sc-server`'s `handlers::file_stores` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // A file store, like an application, exists only as its stored row (§9,
    // §14.1); these endpoints are the row ⇄ connected-store path the SPA drives.
    // Create/update/delete manage the definition and keep the live registry in
    // step — a renamed store's old handle is disconnected, a deleted store's
    // handle too, so nothing goes on serving a store the admin has removed.
    //
    // Note the addressing split, which is deliberate: these operate on a store's
    // **id**, because the row's identity survives a rename, while the file
    // manager below operates on a store's **name**, because that is what an
    // admin picked and what everything else references.

    // Every *defined* store — not merely every connected one — with whether it
    // is currently connected and, if not, why. A store whose directory has been
    // unmounted must still be listed and editable: editing it is the repair.
    set.register(
        Endpoint::new(ids::LIST_FILE_STORES, Method::Get, api().lit("file-stores"))
            .output(TypeSchema::array(file_store_schema())),
    );

    set.register(
        Endpoint::new(
            ids::CREATE_FILE_STORE,
            Method::Post,
            api().lit("file-stores"),
        )
        .input(file_store_input_schema())
        .output(file_store_schema()),
    );

    set.register(
        Endpoint::new(
            ids::UPDATE_FILE_STORE,
            Method::Put,
            api().lit("file-stores").param("id", ValueType::Uuid),
        )
        .input(file_store_input_schema())
        .output(file_store_schema()),
    );

    set.register(
        Endpoint::new(
            ids::DELETE_FILE_STORE,
            Method::Delete,
            api().lit("file-stores").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )])),
    );

    // The registered backends with their settings spec, so the create/edit form
    // can render controls for a backend it knows nothing about — the same move
    // `listFrameworks` makes for frameworks (§13.3).
    set.register(
        Endpoint::new(
            ids::LIST_FILE_STORE_BACKENDS,
            Method::Get,
            api().lit("file-store-backends"),
        )
        .output(TypeSchema::array(backend_info_schema())),
    );

    // Some backends offer *acts* as well as settings — a git store generates a
    // deploy key, clones, pulls, pushes and commits. Those are declared as data
    // (`Operation`, §6.2) exactly as settings are, and run through these two
    // endpoints, so the admin UI renders a button per declared operation and
    // knows nothing about any particular one. A backend supplied by a plugin
    // gets its buttons the same way a built-in one does; without this the UI
    // would need a branch per backend, and an operation would be something only
    // a built-in backend could have.
    //
    // There are two endpoints because there are two scopes, and the difference
    // is real rather than bookkeeping:

    // **Configure scope** — runs against configuration the admin is still
    // editing, so it is addressed by *backend name* and carries the unsaved
    // config in its body. This is what lets "generate a deploy key" happen
    // before the store exists, which it must: saving a git store clones it, and
    // cloning needs a key the remote already accepts. What comes back is the
    // config with the operation's changes merged in, for the form to adopt.
    set.register(
        Endpoint::new(
            ids::RUN_BACKEND_OPERATION,
            Method::Post,
            api()
                .lit("file-store-backends")
                .param("backend", ValueType::Text)
                .lit("operations")
                .param("operation", ValueType::Text),
        )
        .input(TypeSchema::struct_of([
            StructField::new("name", TypeSchema::text()),
            StructField::new("config", TypeSchema::json()),
            StructField::new("input", TypeSchema::json()),
        ]))
        .output(TypeSchema::struct_of([
            StructField::new("config", TypeSchema::json()),
            StructField::new("output", TypeSchema::text()),
            StructField::new("data", TypeSchema::optional(TypeSchema::json())),
        ])),
    );

    // **Instance scope** — runs against a saved store, addressed by id like the
    // rest of the configuration endpoints. Anything the operation changed in the
    // definition is persisted, and the store is reconnected afterwards, since an
    // operation may be exactly what makes it connectable (a clone).
    //
    // It works from the stored *definition*, not from a connected instance, and
    // that is the point: a git store that has never been cloned has no instance,
    // and cloning it is the operation that would otherwise be unreachable.
    set.register(
        Endpoint::new(
            ids::RUN_FILE_STORE_OPERATION,
            Method::Post,
            api()
                .lit("file-stores")
                .param("id", ValueType::Uuid)
                .lit("operations")
                .param("operation", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "input",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::struct_of([
            StructField::new("config", TypeSchema::json()),
            StructField::new("output", TypeSchema::text()),
            // Optional, and shaped by whichever backend filled it: `output` is
            // what every client renders, and this is for the one that has to act
            // on the result rather than show it — the IDE's source-control view,
            // which cannot list changed files from a paragraph of prose (§12.1).
            StructField::new("data", TypeSchema::optional(TypeSchema::json())),
            StructField::new("connected", TypeSchema::bool()),
        ])),
    );
}

/// The fields common to a file store on the wire — everything but its id.
///
/// `config` is opaque JSON: it is whatever the chosen backend's `config_spec`
/// declares, and the API cannot know that statically any more than it can know a
/// framework's settings (see [`framework_ref_schema`]).
fn file_store_fields() -> Vec<StructField> {
    vec![
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("backend", TypeSchema::text()),
        StructField::new("config", TypeSchema::json()),
        // Null means unrestricted, which is distinct from any particular role.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
    ]
}

/// A file store as reported to the admin UI: its definition, plus the live state
/// that only the running server knows.
///
/// The trailing fields are why a store is more than its row. `connected` and
/// `error` exist because a definition can be perfectly valid and still not
/// usable — a directory unmounted since it was saved — and the UI has to show
/// that state with its reason rather than silently omitting the store or
/// pretending it works. `is_git_repo` is a property of the connected instance,
/// so it is null when there is no instance to ask.
///
/// **`id` is nullable, and that is the interesting case.** A store connected by
/// the `--file-store` flag is real and usable but has no row (§1.3: the flag is
/// deliberately ephemeral), so it has no id. Listing only stored definitions
/// would hide it — a developer running with the flag would see an empty store
/// list and no store to browse — so the listing is the *union* of defined and
/// connected stores. A null id is precisely what tells the UI that a store
/// cannot be edited or deleted: there is no row to edit, and it will be gone on
/// the next boot unless the flag is passed again.
fn file_store_schema() -> TypeSchema {
    let mut fields = vec![StructField::new(
        "id",
        TypeSchema::optional(TypeSchema::uuid()),
    )];
    fields.extend(file_store_fields());
    fields.extend([
        StructField::new("connected", TypeSchema::bool()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("is_git_repo", TypeSchema::optional(TypeSchema::bool())),
    ]);
    TypeSchema::Struct(fields)
}

/// The body accepted when creating or updating a file store: the definition's
/// fields minus the id (server-assigned on create, taken from the path on
/// update) and minus the live state, which is observed rather than set.
fn file_store_input_schema() -> TypeSchema {
    TypeSchema::Struct(file_store_fields())
}

/// A registered file-store backend and the settings it declares, so the admin UI
/// can render a form for a backend it knows nothing about. Each setting is a
/// [`form_field_schema`] — the same `FormField` vocabulary a row editor and a
/// framework's settings use.
fn backend_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("operations", TypeSchema::array(operation_schema())),
    ])
}

/// One [`Operation`](sc_types::Operation) a backend declares: an *act* it
/// offers, as opposed to a setting it takes.
///
/// The same "as data" move `form_field_schema` makes, for the other half of
/// what an extension can offer. `scope` says whether it runs against unsaved
/// configuration (`configure`) or a saved store (`instance`), which is what
/// tells the UI where to put the button; `input_spec` is whatever the operation
/// asks the admin for, in the ordinary settings vocabulary, so the same code
/// renders a commit-message box that renders a store's settings.
fn operation_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("scope", TypeSchema::text()),
        StructField::new("input_spec", TypeSchema::array(form_field_schema())),
        StructField::new("on_create", TypeSchema::bool()),
        StructField::new("automatic", TypeSchema::bool()),
    ])
}
